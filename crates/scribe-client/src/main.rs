//! Scribe terminal client -- multi-pane winit + wgpu terminal emulator.

mod ai_indicator;
mod clipboard_cleanup;
mod close_dialog;
mod command_palette;
mod config;
mod context_menu;
mod divider;
mod input;
mod ipc_client;
mod layout;
mod mouse_reporting;
mod mouse_state;
mod notification_dispatcher;
mod notifications;
mod pane;
mod prompt_bar;
mod restore_replay;
mod restore_state;
mod scrollbar;
mod search_overlay;
mod selection;
mod splash;
mod split_scroll;
mod status_bar;
mod sys_stats;
mod tab_bar;
mod tooltip;
mod update_dialog;
mod url_detect;
mod window_state;
mod workspace_layout;
#[cfg(target_os = "linux")]
mod x11_focus;

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant, SystemTime};

use alacritty_terminal::Term;
use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions as _;
use scribe_common::ai_state::AiProvider;
use scribe_common::app::{current_identity, current_state_dir};
use scribe_common::config::{
    ClaudeStatesConfig, ContentPadding, NotificationsConfig, ScribeConfig, resolve_theme,
};
use scribe_common::ids::{SessionId, WindowId, WorkspaceId};
use scribe_common::protocol::{
    AutomationAction, PromptMarkKind, SearchMatch, TerminalSize, UpdateProgressState,
};
use scribe_common::theme::Theme;
use scribe_renderer::types::{CellInstance, GridSize};
use scribe_renderer::{RenderResources, TerminalRenderOptions, TerminalRenderer};
use winit::application::ApplicationHandler;
use winit::dpi::Pixel as _;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::Window;

use crate::ai_indicator::AiStateTracker;
use crate::divider::DividerDrag;
use crate::input::{KeyAction, LayoutAction};
use crate::ipc_client::{ClientCommand, UiEvent};
use crate::layout::{PaneEdges, PaneId, Rect};
use crate::pane::{FeedOutputResult, Pane};
use crate::workspace_layout::WindowLayout;

#[cfg(target_os = "macos")]
fn is_macos_close_window_shortcut(
    event: &winit::event::KeyEvent,
    modifiers: ModifiersState,
) -> bool {
    event.state.is_pressed()
        && !event.repeat
        && modifiers.super_key()
        && !modifiers.control_key()
        && !modifiers.alt_key()
        && !modifiers.shift_key()
        && matches!(&event.logical_key, Key::Character(ch) if ch.eq_ignore_ascii_case("w"))
}

#[cfg(not(target_os = "macos"))]
fn is_macos_close_window_shortcut(
    _event: &winit::event::KeyEvent,
    _modifiers: ModifiersState,
) -> bool {
    false
}

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DrainQueueState {
    Drained,
    MoreQueued,
}

impl DrainQueueState {
    const fn has_more(self) -> bool {
        matches!(self, Self::MoreQueued)
    }

    const fn from_has_more(has_more: bool) -> Self {
        if has_more { Self::MoreQueued } else { Self::Drained }
    }
}

struct PendingOutputDrainResult {
    needs_redraw: bool,
    queue_state: DrainQueueState,
    sync_pending: bool,
}

type WorkspaceTerminalSizeMap = HashMap<WorkspaceId, (u16, u16)>;
type WorkspaceRectMap = HashMap<WorkspaceId, Rect>;
type WorkspaceGridMap = HashMap<WorkspaceId, GridSize>;
type WorkspaceDimensionMaps = (WorkspaceTerminalSizeMap, WorkspaceRectMap, WorkspaceGridMap);

/// Once a pane accumulates more than this many committed PTY bursts, stop
/// replaying them one redraw at a time and catch up to the latest committed
/// terminal state instead.
const OUTPUT_FRAME_CATCH_UP_THRESHOLD: usize = 4;
const BRACKETED_PASTE_START: &[u8] = b"\x1b[200~";
const BRACKETED_PASTE_END: &[u8] = b"\x1b[201~";
const MAX_KEY_INPUT_CHUNK: usize = 4 * 1024;

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

/// State for an in-progress pane title pill drag-to-rearrange operation.
struct PaneDrag {
    /// Workspace the dragged pane belongs to.
    workspace_id: WorkspaceId,
    /// The pane being dragged.
    pane_id: PaneId,
    /// Cursor X at drag start (used for threshold detection).
    start_x: f32,
    /// Cursor Y at drag start (used for threshold detection).
    start_y: f32,
    /// `true` once the cursor has moved more than 5 px from the start.
    dragging: bool,
}

struct SplitPanePlan {
    workspace_id: WorkspaceId,
    new_pane_id: PaneId,
    session_id: SessionId,
    inherited_cwd: Option<std::path::PathBuf>,
    rects: Vec<(PaneId, Rect, PaneEdges)>,
    ws_rects: Vec<(WorkspaceId, Rect)>,
    new_rect: Rect,
    new_edges: PaneEdges,
    grid: GridSize,
}

struct PasteTarget {
    session_id: SessionId,
    bracketed: bool,
}

struct WorkspaceInfoUpdate {
    workspace_id: WorkspaceId,
    name: Option<String>,
    accent_color: String,
    split_direction: Option<scribe_common::protocol::LayoutDirection>,
    project_root: Option<std::path::PathBuf>,
}

#[derive(Clone, Copy)]
struct ReconnectRestoreContext<'a> {
    ws_rects_map: &'a HashMap<WorkspaceId, Rect>,
    fallback_viewport: Rect,
    tab_pane_trees: &'a HashMap<SessionId, scribe_common::protocol::PaneTreeNode>,
    metadata: &'a SessionMetadataMap<'a>,
}

#[derive(Clone, Copy)]
struct TabRestoreRequest<'a> {
    workspace_id: WorkspaceId,
    session_id: SessionId,
    workspace_rect: Rect,
    tab_count: usize,
    pane_tree: Option<&'a scribe_common::protocol::PaneTreeNode>,
    metadata: &'a SessionMetadataMap<'a>,
}

type SessionMetadata<'a> = (
    Option<&'a str>,
    Option<&'a str>,
    Option<&'a std::path::PathBuf>,
    Option<&'a scribe_common::protocol::SessionContext>,
    Option<&'a str>,
    Option<AiProvider>,
    Option<&'a str>,
);
type SessionMetadataMap<'a> = HashMap<SessionId, SessionMetadata<'a>>;
type WorkspaceRects = Vec<(WorkspaceId, Rect)>;
type MousePressContext = (Rect, WorkspaceRects);

#[derive(Clone)]
struct CommandPaletteEntry {
    label: String,
    action: AutomationAction,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingShutdown {
    CloseWindow { window_id: WindowId },
    QuitAll,
}

struct AppStartup {
    config: ScribeConfig,
    theme: Theme,
    window_layout: WindowLayout,
    cursor_blink_enabled: bool,
    opacity: f32,
    window_transparent: bool,
    bindings: input::Bindings,
    window_registry: window_state::WindowRegistry,
    saved_geometry: Option<window_state::WindowGeometry>,
    ai_tracker: AiStateTracker,
    notification_tracker: notifications::NotificationTracker,
    clipboard: Option<arboard::Clipboard>,
    hostname: String,
    sys_stats: sys_stats::SystemStatsCollector,
    config_watcher: Option<notify::RecommendedWatcher>,
}

impl AppStartup {
    fn load(proxy: &EventLoopProxy<UiEvent>, window_id: Option<WindowId>) -> Self {
        let config = scribe_common::config::load_config().unwrap_or_else(|error| {
            tracing::warn!("failed to load config: {error}, using defaults");
            ScribeConfig::default()
        });
        let theme = resolve_theme(&config);
        let window_layout = WindowLayout::new(WorkspaceId::new(), Some(theme.chrome.accent));
        let window_registry = window_state::WindowRegistry::new();

        Self {
            cursor_blink_enabled: config.appearance.cursor_blink,
            opacity: config.appearance.opacity,
            window_transparent: config.appearance.opacity < 1.0,
            bindings: input::Bindings::parse(&config.keybindings),
            saved_geometry: window_id.map(|wid| window_registry.load(wid)),
            ai_tracker: AiStateTracker::new(config.terminal.ai_session.claude_states.clone()),
            notification_tracker: notifications::NotificationTracker::new(
                config.notifications.clone(),
            ),
            clipboard: arboard::Clipboard::new()
                .map_err(|error| {
                    tracing::warn!("clipboard unavailable: {error}");
                })
                .ok(),
            hostname: read_hostname(),
            sys_stats: sys_stats::SystemStatsCollector::new(),
            config_watcher: config::start_config_watcher(proxy.clone()),
            config,
            theme,
            window_layout,
            window_registry,
        }
    }
}

struct ConfigReloadPlan {
    changes: u8,
}

impl ConfigReloadPlan {
    const THEME_CHANGED: u8 = 1 << 0;
    const FONT_CHANGED: u8 = 1 << 1;
    const HIDE_CODEX_LOGS_CHANGED: u8 = 1 << 2;
    const OPACITY_CHANGED: u8 = 1 << 3;
    const TAB_BAR_CHANGED: u8 = 1 << 4;
    const PADDING_CHANGED: u8 = 1 << 5;
    const PROMPT_BAR_CHANGED: u8 = 1 << 6;

    fn analyze(old: &ScribeConfig, new: &ScribeConfig) -> Self {
        let mut plan = Self { changes: 0 };
        plan.set(Self::THEME_CHANGED, theme_reload_needed(old, new));
        plan.set(Self::FONT_CHANGED, font_params_changed(old, new));
        plan.set(
            Self::HIDE_CODEX_LOGS_CHANGED,
            old.terminal.ai_session.hide_codex_hook_logs
                != new.terminal.ai_session.hide_codex_hook_logs,
        );
        plan.set(
            Self::OPACITY_CHANGED,
            (old.appearance.opacity - new.appearance.opacity).abs() > f32::EPSILON,
        );
        plan.set(Self::TAB_BAR_CHANGED, tab_bar_metrics_changed(old, new));
        plan.set(Self::PADDING_CHANGED, content_padding_changed(old, new));
        plan.set(
            Self::PROMPT_BAR_CHANGED,
            old.terminal.prompt_bar.enabled != new.terminal.prompt_bar.enabled,
        );
        plan
    }

    fn set(&mut self, change: u8, enabled: bool) {
        if enabled {
            self.changes |= change;
        }
    }

    const fn contains(&self, change: u8) -> bool {
        self.changes & change != 0
    }

    fn theme_changed(&self) -> bool {
        self.contains(Self::THEME_CHANGED)
    }

    fn font_changed(&self) -> bool {
        self.contains(Self::FONT_CHANGED)
    }

    fn hide_codex_logs_changed(&self) -> bool {
        self.contains(Self::HIDE_CODEX_LOGS_CHANGED)
    }

    fn opacity_changed(&self) -> bool {
        self.contains(Self::OPACITY_CHANGED)
    }

    fn prompt_bar_changed(&self) -> bool {
        self.contains(Self::PROMPT_BAR_CHANGED)
    }

    fn needs_layout_resize(&self) -> bool {
        self.contains(Self::FONT_CHANGED)
            || self.contains(Self::TAB_BAR_CHANGED)
            || self.contains(Self::PADDING_CHANGED)
    }
}

fn theme_reload_needed(old: &ScribeConfig, new: &ScribeConfig) -> bool {
    let theme_name_changed = old.appearance.theme != new.appearance.theme;
    let inline_theme_changed = old.theme != new.theme;
    let external_theme_selected = new.appearance.theme != "custom"
        && scribe_common::theme::resolve_preset(&new.appearance.theme).is_none();
    theme_name_changed || inline_theme_changed || external_theme_selected
}

fn font_params_changed(old: &ScribeConfig, new: &ScribeConfig) -> bool {
    old.appearance.font != new.appearance.font
        || (old.appearance.font_size - new.appearance.font_size).abs() > f32::EPSILON
        || old.appearance.font_weight != new.appearance.font_weight
        || old.appearance.font_weight_bold != new.appearance.font_weight_bold
        || old.appearance.ligatures != new.appearance.ligatures
        || old.appearance.line_padding != new.appearance.line_padding
}

fn tab_bar_metrics_changed(old: &ScribeConfig, new: &ScribeConfig) -> bool {
    (old.appearance.tab_bar_padding - new.appearance.tab_bar_padding).abs() > f32::EPSILON
        || old.appearance.tab_width != new.appearance.tab_width
        || (old.appearance.tab_height - new.appearance.tab_height).abs() > f32::EPSILON
        || (old.appearance.status_bar_height - new.appearance.status_bar_height).abs()
            > f32::EPSILON
}

fn content_padding_changed(old: &ScribeConfig, new: &ScribeConfig) -> bool {
    let old_pad = &old.appearance.content_padding;
    let new_pad = &new.appearance.content_padding;
    (old_pad.top - new_pad.top).abs() > f32::EPSILON
        || (old_pad.right - new_pad.right).abs() > f32::EPSILON
        || (old_pad.bottom - new_pad.bottom).abs() > f32::EPSILON
        || (old_pad.left - new_pad.left).abs() > f32::EPSILON
}

fn shift_tab_selection_for_pane(
    tab: &mut workspace_layout::TabState,
    pane_id: PaneId,
    delta: i32,
    topmost: i32,
) {
    if tab.focused_pane != pane_id {
        return;
    }

    let Some(selection) = &mut tab.selection else { return };
    selection.shift_rows(-delta);
    if selection.normalized().0.row < topmost {
        tab.selection = None;
    }
}

fn base_command_palette_entries() -> Vec<CommandPaletteEntry> {
    vec![
        CommandPaletteEntry {
            label: String::from("Open Settings"),
            action: AutomationAction::OpenSettings,
        },
        CommandPaletteEntry {
            label: String::from("Find in Scrollback"),
            action: AutomationAction::OpenFind,
        },
        CommandPaletteEntry { label: String::from("New Tab"), action: AutomationAction::NewTab },
        CommandPaletteEntry {
            label: String::from("New Claude Tab"),
            action: AutomationAction::NewClaudeTab,
        },
        CommandPaletteEntry {
            label: String::from("Resume Claude Tab"),
            action: AutomationAction::NewClaudeResumeTab,
        },
        CommandPaletteEntry {
            label: String::from("New Codex Tab"),
            action: AutomationAction::NewCodexTab,
        },
        CommandPaletteEntry {
            label: String::from("Resume Codex Tab"),
            action: AutomationAction::NewCodexResumeTab,
        },
        CommandPaletteEntry {
            label: String::from("Split Pane Vertical"),
            action: AutomationAction::SplitVertical,
        },
        CommandPaletteEntry {
            label: String::from("Split Pane Horizontal"),
            action: AutomationAction::SplitHorizontal,
        },
        CommandPaletteEntry {
            label: String::from("Close Pane"),
            action: AutomationAction::ClosePane,
        },
        CommandPaletteEntry {
            label: String::from("Close Tab"),
            action: AutomationAction::CloseTab,
        },
        CommandPaletteEntry {
            label: String::from("New Window"),
            action: AutomationAction::NewWindow,
        },
    ]
}

fn profile_command_palette_entries(active_profile: Option<&str>) -> Vec<CommandPaletteEntry> {
    let Ok(profile_names) = scribe_common::profiles::list_profiles() else { return Vec::new() };
    profile_names
        .into_iter()
        .map(|name| {
            let is_active_profile = active_profile == Some(name.as_str());
            let mut label = format!("Switch Profile: {name}");
            if is_active_profile {
                label.push_str(" (active)");
            }
            CommandPaletteEntry { label, action: AutomationAction::SwitchProfile { name } }
        })
        .collect()
}

fn terminal_content_hit(
    tab: &workspace_layout::TabState,
    ws_rect: Rect,
    x: f32,
    y: f32,
    tab_bar_h: f32,
) -> bool {
    tab.pane_layout.compute_rects(ws_rect).iter().any(|(_, pane_rect, _)| {
        let content_top = pane_rect.y + tab_bar_h;
        x >= pane_rect.x
            && x < pane_rect.x + pane_rect.width
            && y >= content_top
            && y < pane_rect.y + pane_rect.height
    })
}

fn sync_pane_resize_state(
    pane: &mut Pane,
    tx: &Sender<ClientCommand>,
    cell_size: Option<(u16, u16)>,
) {
    use alacritty_terminal::grid::Dimensions as _;

    if pane.last_sent_grid != Some(pane.grid) {
        if let Some((cell_width, cell_height)) = cell_size {
            let size = TerminalSize {
                cols: pane.grid.cols,
                rows: pane.grid.rows,
                cell_width,
                cell_height,
            };
            send_resize(tx, pane.session_id, size);
        }
        pane.last_sent_grid = Some(pane.grid);
    }

    if pane.term.columns() != usize::from(pane.grid.cols)
        || pane.term.screen_lines() != usize::from(pane.grid.rows)
    {
        pane.resize_term_only(pane.grid.cols, pane.grid.rows);
    }
}

struct AppLaunchState {
    explicit_new_window: bool,
    restore_spawn_child: bool,
}

struct AppConnectionState {
    server_connected: bool,
    quit_restore_cleared: bool,
}

struct AppAnimationState {
    running: bool,
    generation: Arc<AtomicU64>,
}

struct AppSplashState {
    active: bool,
    content_ready: bool,
}

struct AppSplashLoadState {
    needs_list_sessions: bool,
}

struct AppCursorState {
    visible: bool,
    blink_enabled: bool,
}

struct AppWindowFocusState {
    window_focused: bool,
}

/// Application state for the winit event loop.
struct App {
    // Window identity
    /// Window ID from CLI arg (if provided) or assigned by the server.
    window_id: Option<WindowId>,
    launch: AppLaunchState,

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
    /// Line bounds from the initial triple-click (for drag-by-line).
    line_drag_anchor: Option<(selection::SelectionPoint, selection::SelectionPoint)>,

    // Connection state
    connection: AppConnectionState,
    /// Destructive shutdown requested by this window and awaiting server acknowledgment.
    pending_shutdown: Option<PendingShutdown>,
    /// Persisted logical restore state for this client window.
    restore_store: restore_state::RestoreStore,
    /// Debounce marker for the next restore snapshot write.
    restore_save_pending: Option<Instant>,

    // AI state
    ai_tracker: AiStateTracker,
    notification_tracker: notifications::NotificationTracker,
    /// Sender to the platform notification dispatcher thread.
    /// Initialised alongside the IPC thread in `resumed`. `None`
    /// before the dispatcher has been spawned.
    notification_tx: Option<tokio::sync::mpsc::UnboundedSender<notification_dispatcher::NotifReq>>,
    animation: AppAnimationState,

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
    command_palette: command_palette::CommandPalette,
    command_palette_items: Vec<CommandPaletteEntry>,
    search_overlay: search_overlay::SearchOverlay,

    // Close dialog overlay (shown on window close request)
    close_dialog: Option<close_dialog::CloseDialog>,

    // Update state
    /// Available update version and URL. Set by `UpdateAvailable`, cleared on dismiss.
    update_available: Option<String>,
    /// Current update progress state. Set by `UpdateProgress`, cleared on completion/failure.
    update_progress: Option<UpdateProgressState>,
    /// Active update confirmation dialog (shown when user clicks the update button).
    update_dialog: Option<update_dialog::UpdateDialog>,
    /// Clickable update segment rect in the window status bar (updated each frame).
    status_bar_update_rect: Option<layout::Rect>,

    // Context menu overlay (shown on right-click)
    context_menu: Option<context_menu::ContextMenu>,

    splash: AppSplashState,
    splash_load: AppSplashLoadState,

    /// Instant when the splash first rendered, used to enforce a minimum
    /// display duration so the compositor has time to present it.
    splash_first_rendered: Option<Instant>,

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

    cursor: AppCursorState,
    focus: AppWindowFocusState,
    /// X11 active-window guard — suppresses key events during compositor overlays.
    #[cfg(target_os = "linux")]
    x11_focus_guard: Option<x11_focus::X11FocusGuard>,
    /// Time of last blink toggle.
    blink_timer: Instant,

    /// Current opacity (0.0-1.0). Applied to clear color and cell backgrounds.
    opacity: f32,
    /// Whether the window was created with transparency support.
    window_transparent: bool,
    /// Display scale factor (1.0 on standard displays, 2.0 on Retina/HiDPI).
    /// Font sizes are multiplied by this to produce crisp glyphs at the
    /// native physical resolution.
    scale_factor: f32,

    /// Per-window geometry registry (multi-window support).
    window_registry: window_state::WindowRegistry,
    /// Loaded geometry to apply during init (consumed once).
    saved_geometry: Option<window_state::WindowGeometry>,
    /// When set, a geometry save is pending (debounced).
    geometry_save_pending: Option<Instant>,
    /// When set, a resize IPC flush is pending (debounced).
    resize_pending: Option<Instant>,

    /// Workspace IDs that still need a one-time parent split-direction patch
    /// from legacy reconnect fallback (`SessionList` without a workspace tree).
    legacy_workspace_direction_updates: HashSet<WorkspaceId>,
    /// Clickable tab rects `(workspace_id, tab_index, rect)` (updated each frame).
    tab_hit_targets: Vec<(WorkspaceId, usize, layout::Rect)>,
    /// Close button rects `(workspace_id, tab_index, rect)` (updated each frame).
    tab_close_hit_targets: Vec<(WorkspaceId, usize, layout::Rect)>,
    /// Which tab's close button is currently hovered: `(workspace_id, tab_index)`.
    hovered_tab_close: Option<(WorkspaceId, usize)>,
    /// Which tab is currently hovered (for background highlight): `(workspace_id, tab_index)`.
    hovered_tab: Option<(WorkspaceId, usize)>,
    /// Active tab drag state for reordering.
    tab_drag: Option<TabDrag>,
    /// Per-tab pixel X offsets for the slide animation on the drag workspace.
    tab_drag_offsets: Vec<f32>,
    /// Active pane title pill drag state for rearranging panes.
    pane_drag: Option<PaneDrag>,
    /// Clickable equalize rects from tab bars `(workspace_id, rect)` (updated each frame).
    tab_bar_equalize_targets: Vec<(WorkspaceId, layout::Rect)>,
    /// Clickable rect for the status bar gear icon (updated each frame).
    status_bar_gear_rect: Option<layout::Rect>,
    /// Clickable rect for the status bar equalize icon (updated each frame).
    status_bar_equalize_rect: Option<layout::Rect>,

    /// Tooltip hover targets for the status bar (updated each frame).
    status_bar_tooltip_targets: Vec<tooltip::TooltipAnchor>,
    /// Tooltip hover targets for tab bars across all workspaces (updated each frame).
    tab_bar_tooltip_targets: Vec<tooltip::TooltipAnchor>,
    /// Active tooltip state: (text, `anchor_rect`, position).
    active_tooltip: Option<(String, layout::Rect, tooltip::TooltipPosition)>,

    /// Prompt bar hover state: which pane and which line the cursor is over.
    prompt_bar_hover: Option<(PaneId, prompt_bar::PromptBarHover)>,
    /// Prompt bar pressed state: which pane and which target is currently active.
    prompt_bar_pressed: Option<(PaneId, prompt_bar::PromptBarHover)>,

    /// Split-scroll jump button hover state.
    scroll_pin_hover: Option<PaneId>,

    /// System hostname for the window-level status bar (fetched once at startup).
    hostname: String,
    /// System resource stats collector for the status bar.
    sys_stats: sys_stats::SystemStatsCollector,

    /// Pending PTY output bytes, accumulated per session and drained in
    /// `about_to_wait`. Coalescing output processing ensures input events
    /// are never blocked behind a queue of `PtyOutput` messages.
    pending_pty_bytes: HashMap<SessionId, Vec<u8>>,

    /// Per-pane URL span caches (dirty-flag lazy refresh).
    url_caches: HashMap<PaneId, url_detect::PaneUrlCache>,
    /// The URL span the cursor is currently hovering over, if any.
    hovered_url: Option<url_detect::UrlSpan>,

    /// Config file watcher -- kept alive for its side-effect of sending
    /// `UiEvent::ConfigChanged` events.
    config_watcher_keepalive: Option<notify::RecommendedWatcher>,
}

impl App {
    fn new(
        wgpu_instance: wgpu::Instance,
        proxy: EventLoopProxy<UiEvent>,
        window_id: Option<WindowId>,
        restore_spawn_child: bool,
    ) -> Self {
        let startup = AppStartup::load(&proxy, window_id);
        let animation_proxy = proxy.clone();
        let mut app =
            Self::new_base(wgpu_instance, proxy, animation_proxy, window_id, restore_spawn_child);
        app.apply_startup(startup);
        app
    }

    fn new_base(
        wgpu_instance: wgpu::Instance,
        proxy: EventLoopProxy<UiEvent>,
        animation_proxy: EventLoopProxy<UiEvent>,
        window_id: Option<WindowId>,
        restore_spawn_child: bool,
    ) -> Self {
        let config = ScribeConfig::default();
        let theme = resolve_theme(&config);
        let window_layout = WindowLayout::new(WorkspaceId::new(), Some(theme.chrome.accent));
        let cursor_blink_enabled = config.appearance.cursor_blink;
        let opacity = config.appearance.opacity;
        let window_transparent = opacity < 1.0;
        let bindings = input::Bindings::parse(&config.keybindings);

        let mut app = Self {
            window_id,
            launch: AppLaunchState {
                explicit_new_window: window_id.is_some(),
                restore_spawn_child,
            },
            config,
            theme,
            window_layout,
            bindings,
            opacity,
            window_transparent,
            ..Self::base_defaults(wgpu_instance, proxy, animation_proxy)
        };
        app.cursor.blink_enabled = cursor_blink_enabled;
        app
    }

    #[rustfmt::skip]
    fn base_defaults(
        wgpu_instance: wgpu::Instance,
        proxy: EventLoopProxy<UiEvent>,
        animation_proxy: EventLoopProxy<UiEvent>,
    ) -> Self {
        let config = ScribeConfig::default();
        let theme = resolve_theme(&config);
        let window_layout = WindowLayout::new(WorkspaceId::new(), Some(theme.chrome.accent));
        let bindings = input::Bindings::parse(&config.keybindings);

        Self {
            window_id: None,
            launch: AppLaunchState { explicit_new_window: false, restore_spawn_child: false },
            config, theme, window: None, gpu: None, cmd_tx: None, window_layout,
            panes: HashMap::new(), session_to_pane: HashMap::new(), pending_sessions: VecDeque::new(),
            divider_drag: None, workspace_divider_drag: None, scrollbar_drag_pane: None,
            active_selection: None, mouse_selecting: false, mouse_click: mouse_state::MouseClickState::new(),
            word_drag_anchor: None, line_drag_anchor: None,
            connection: AppConnectionState { server_connected: false, quit_restore_cleared: false },
            pending_shutdown: None, restore_store: restore_state::RestoreStore::new(), restore_save_pending: None,
            ai_tracker: AiStateTracker::new(ClaudeStatesConfig::default()),
            notification_tracker: notifications::NotificationTracker::new(NotificationsConfig::default()),
            notification_tx: None,
            animation: AppAnimationState {
                running: false,
                generation: Arc::new(AtomicU64::new(0)),
            },
            modifiers: ModifiersState::default(),
            bindings, clipboard: None, zoom_level: 0,
            command_palette: command_palette::CommandPalette::new(), command_palette_items: Vec::new(),
            search_overlay: search_overlay::SearchOverlay::new(),
            close_dialog: None, update_available: None, update_progress: None, update_dialog: None,
            status_bar_update_rect: None, context_menu: None,
            splash: AppSplashState { active: true, content_ready: false },
            splash_load: AppSplashLoadState { needs_list_sessions: true },
            splash_first_rendered: None,
            wgpu_instance, proxy: Some(proxy), animation_proxy: Some(animation_proxy),
            last_cursor_pos: None, last_tick: Instant::now(),
            cursor: AppCursorState { visible: true, blink_enabled: false },
            focus: AppWindowFocusState { window_focused: true },
            #[cfg(target_os = "linux")]
            x11_focus_guard: None,
            blink_timer: Instant::now(), opacity: 1.0, window_transparent: false, scale_factor: 1.0,
            window_registry: window_state::WindowRegistry::new(), saved_geometry: None,
            geometry_save_pending: None, resize_pending: None, legacy_workspace_direction_updates: HashSet::new(),
            tab_hit_targets: Vec::new(), tab_close_hit_targets: Vec::new(), hovered_tab_close: None, hovered_tab: None,
            tab_drag: None, tab_drag_offsets: Vec::new(), pane_drag: None, tab_bar_equalize_targets: Vec::new(),
            status_bar_gear_rect: None, status_bar_equalize_rect: None,
            status_bar_tooltip_targets: Vec::new(), tab_bar_tooltip_targets: Vec::new(), active_tooltip: None,
            prompt_bar_hover: None, prompt_bar_pressed: None, scroll_pin_hover: None,
            hostname: String::new(), sys_stats: sys_stats::SystemStatsCollector::new(),
            pending_pty_bytes: HashMap::new(), url_caches: HashMap::new(), hovered_url: None, config_watcher_keepalive: None,
        }
    }

    fn apply_startup(&mut self, startup: AppStartup) {
        self.config = startup.config;
        self.theme = startup.theme;
        self.window_layout = startup.window_layout;
        self.ai_tracker = startup.ai_tracker;
        self.notification_tracker = startup.notification_tracker;
        self.bindings = startup.bindings;
        self.clipboard = startup.clipboard;
        self.cursor.blink_enabled = startup.cursor_blink_enabled;
        self.opacity = startup.opacity;
        self.window_transparent = startup.window_transparent;
        self.window_registry = startup.window_registry;
        self.saved_geometry = startup.saved_geometry;
        self.hostname = startup.hostname;
        self.sys_stats = startup.sys_stats;
        self.config_watcher_keepalive = startup.config_watcher;
        let _ = self.config_watcher_keepalive.as_ref();
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
        let _ = self.handle_stream_user_event(&event)
            || self.handle_session_metadata_user_event(event_loop, &event)
            || self.handle_ai_prompt_user_event(&event)
            || self.handle_workspace_user_event(event_loop, &event)
            || self.handle_lifecycle_user_event(event_loop, &event)
            || self.handle_update_user_event(&event);
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // Drain coalesced PTY output — all input events have already been
        // processed, so keystrokes are never blocked behind output messages.
        self.drain_pending_pty_output();

        if self.has_pending_output_frames() {
            self.request_redraw();
            event_loop.set_control_flow(ControlFlow::Poll);
        } else if let Some(deadline) = self.next_idle_wake_deadline() {
            event_loop.set_control_flow(ControlFlow::WaitUntil(deadline));
        }
        // When no deadline is returned, leave ControlFlow at winit's default
        // (Wait). `next_idle_wake_deadline` already calls `request_redraw`
        // for any branch that fires immediately.

        // Keep the X11 active-window guard up to date so it can detect
        // compositor overlays even when no key events are arriving.
        #[cfg(target_os = "linux")]
        if let Some(guard) = &mut self.x11_focus_guard {
            guard.poll();
        }

        self.flush_geometry_if_due();
        self.flush_resize_if_pending();
        self.flush_restore_if_due();
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: winit::window::WindowId,
        event: WindowEvent,
    ) {
        if self.handle_modal_window_event(event_loop, &event) {
            return;
        }

        self.handle_main_window_event(event_loop, &event);
    }
}

impl App {
    fn handle_stream_user_event(&mut self, event: &UiEvent) -> bool {
        match event {
            UiEvent::PtyOutput { session_id, data } => {
                self.pending_pty_bytes.entry(*session_id).or_default().extend_from_slice(data);
                true
            }
            UiEvent::ScreenSnapshot { session_id, snapshot } => {
                self.handle_screen_snapshot_event(*session_id, snapshot);
                true
            }
            UiEvent::SessionReplay { session_id, replay } => {
                self.handle_session_replay_event(*session_id, replay);
                true
            }
            UiEvent::SearchResults { session_id, query, matches } => {
                if self.focused_session_id() == Some(*session_id)
                    && self.search_overlay.is_active()
                    && self.search_overlay.query() == *query
                {
                    self.search_overlay.set_results(matches.clone());
                    self.scroll_focused_pane_to_search_match();
                    self.request_redraw();
                }
                true
            }
            UiEvent::TrimScrollback { session_id, history_rows } => {
                self.handle_trim_scrollback_event(*session_id, *history_rows);
                true
            }
            UiEvent::ScrollBottom { session_id } => {
                self.handle_scroll_bottom_event(*session_id);
                true
            }
            _ => false,
        }
    }

    fn handle_session_metadata_user_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        event: &UiEvent,
    ) -> bool {
        match event {
            UiEvent::SessionCreated { session_id, shell_name } => {
                self.handle_session_created(*session_id, shell_name);
                true
            }
            UiEvent::SessionExited { session_id } => {
                self.handle_session_exited(*session_id, event_loop);
                true
            }
            UiEvent::Bell { session_id } => {
                self.handle_bell_event(*session_id);
                true
            }
            UiEvent::CwdChanged { session_id, cwd } => {
                self.handle_cwd_changed(*session_id, cwd.clone());
                true
            }
            UiEvent::SessionContextChanged { session_id, context } => {
                self.handle_session_context_changed(*session_id, context.clone());
                true
            }
            UiEvent::TitleChanged { session_id, title } => {
                self.handle_title_changed(*session_id, title);
                true
            }
            UiEvent::CodexTaskLabelChanged { session_id, task_label } => {
                self.handle_codex_task_label_changed(*session_id, task_label);
                true
            }
            UiEvent::CodexTaskLabelCleared { session_id } => {
                self.handle_codex_task_label_cleared(*session_id);
                true
            }
            UiEvent::GitBranch { session_id, branch } => {
                self.handle_git_branch(*session_id, branch.clone());
                true
            }
            _ => false,
        }
    }

    fn handle_ai_prompt_user_event(&mut self, event: &UiEvent) -> bool {
        match event {
            UiEvent::AiStateChanged { session_id, ai_state } => {
                self.handle_ai_state_changed(*session_id, ai_state.clone());
                true
            }
            UiEvent::AiStateCleared { session_id } => {
                self.handle_ai_state_cleared(*session_id);
                true
            }
            UiEvent::PromptMark { session_id, kind, click_events } => {
                self.handle_prompt_mark(*session_id, *kind, *click_events);
                true
            }
            UiEvent::PromptReceived { session_id, text } => {
                self.handle_prompt_received(*session_id, text.clone());
                true
            }
            _ => false,
        }
    }

    fn handle_workspace_user_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        event: &UiEvent,
    ) -> bool {
        match event {
            UiEvent::WorkspaceInfo {
                workspace_id,
                name,
                accent_color,
                split_direction,
                project_root,
            } => {
                self.handle_workspace_info(WorkspaceInfoUpdate {
                    workspace_id: *workspace_id,
                    name: name.clone(),
                    accent_color: accent_color.clone(),
                    split_direction: *split_direction,
                    project_root: project_root.clone(),
                });
                true
            }
            UiEvent::SessionList { sessions, workspace_tree, workspaces } => {
                self.handle_session_list(event_loop, sessions, workspace_tree.as_ref(), workspaces);
                true
            }
            UiEvent::WorkspaceNamed { workspace_id, name, project_root } => {
                self.handle_workspace_named(*workspace_id, name, project_root.clone());
                true
            }
            UiEvent::ConfigChanged => {
                self.handle_config_changed();
                true
            }
            _ => false,
        }
    }

    fn handle_lifecycle_user_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        event: &UiEvent,
    ) -> bool {
        match event {
            UiEvent::ServerDisconnected => {
                self.handle_server_disconnected(event_loop);
                true
            }
            UiEvent::AnimationTick => {
                self.handle_animation_tick();
                true
            }
            UiEvent::Welcome { window_id, other_windows } => {
                self.handle_welcome(event_loop, *window_id, other_windows);
                true
            }
            UiEvent::WindowClosed { window_id } => {
                self.handle_window_closed(*window_id, event_loop);
                true
            }
            UiEvent::QuitRequested => {
                self.handle_quit_requested(event_loop);
                true
            }
            UiEvent::RunAction { action } => {
                self.execute_automation_action(action.clone());
                true
            }
            _ => false,
        }
    }

    fn handle_update_user_event(&mut self, event: &UiEvent) -> bool {
        match event {
            UiEvent::UpdateAvailable { version } => {
                self.handle_update_available(version.clone());
                true
            }
            UiEvent::UpdateProgress { state } => {
                self.handle_update_progress(state.clone());
                true
            }
            _ => false,
        }
    }

    fn handle_screen_snapshot_event(
        &mut self,
        session_id: SessionId,
        snapshot: &scribe_common::screen::ScreenSnapshot,
    ) {
        self.pending_pty_bytes.remove(&session_id);
        if let Some(pane_id) = self.session_to_pane.get(&session_id).copied()
            && let Some(pane) = self.panes.get_mut(&pane_id)
        {
            pane.reset_output_queue();
        }
        self.handle_screen_snapshot(session_id, snapshot);
    }

    fn handle_session_replay_event(
        &mut self,
        session_id: SessionId,
        replay: &scribe_common::screen_replay::SessionReplay,
    ) {
        self.pending_pty_bytes.remove(&session_id);
        if let Some(pane_id) = self.session_to_pane.get(&session_id).copied()
            && let Some(pane) = self.panes.get_mut(&pane_id)
        {
            pane.reset_output_queue();
        }
        self.handle_session_replay(session_id, replay);
    }

    fn handle_scroll_bottom_event(&mut self, session_id: SessionId) {
        if let Some(&pane_id) = self.session_to_pane.get(&session_id)
            && let Some(pane) = self.panes.get_mut(&pane_id)
        {
            pane.split_scroll = None;
            pane.term.scroll_display(alacritty_terminal::grid::Scroll::Bottom);
            pane.content_dirty = true;
            self.request_redraw();
        }
    }

    fn handle_trim_scrollback_event(&mut self, session_id: SessionId, history_rows: u32) {
        self.flush_session_output_now(session_id);

        let Some(&pane_id) = self.session_to_pane.get(&session_id) else { return };
        let Some(pane) = self.panes.get_mut(&pane_id) else { return };
        let max_rows = usize::try_from(self.config.terminal.scrollback_lines).unwrap_or(usize::MAX);
        let kept_rows = usize::try_from(history_rows).unwrap_or(usize::MAX);
        trim_term_scrollback(&mut pane.term, kept_rows, max_rows);
        pane.content_dirty = true;
        self.request_redraw();
    }

    fn flush_session_output_now(&mut self, session_id: SessionId) {
        if let Some(bytes) = self.pending_pty_bytes.remove(&session_id) {
            self.handle_pty_output(session_id, &bytes);
        }

        let Some(&pane_id) = self.session_to_pane.get(&session_id) else { return };
        while self.panes.get(&pane_id).is_some_and(|pane| !pane.pending_output_frames.is_empty()) {
            let Some(result) = self.drain_pane_output_until_frame(pane_id) else { break };
            if result.sync_pending {
                self.ensure_animation_running();
            }
        }
    }

    fn handle_bell_event(&mut self, session_id: SessionId) {
        #[cfg(target_os = "linux")]
        if self.notification_tracker.should_suppress_linux_bell_attention(session_id) {
            return;
        }

        if (!self.focus.window_focused || self.focused_session_id() != Some(session_id))
            && self.update_available.is_none()
            && let Some(window) = &self.window
        {
            window.request_user_attention(Some(winit::window::UserAttentionType::Informational));
        }
    }

    fn handle_ai_state_cleared(&mut self, session_id: SessionId) {
        self.ai_tracker.remove(session_id);
        self.notification_tracker.remove(session_id);
        self.close_pending_notification(session_id);
        if let Some(pane_id) = self.session_to_pane.get(&session_id).copied()
            && let Some(pane) = self.panes.get_mut(&pane_id)
            && matches!(pane.launch_binding.kind, restore_state::LaunchKind::Ai { .. })
        {
            // `AiStateCleared` only fires when the tool explicitly goes
            // inactive while the PTY stays alive, which means this pane is
            // back at a normal shell prompt. Persist that reality so cold
            // restart does not reopen Claude/Codex on a plain shell tab.
            pane.launch_binding.kind = restore_state::LaunchKind::Shell;
            self.mark_restore_dirty();
        }
        self.clear_pane_prompts(session_id);
        self.request_redraw();
    }

    fn handle_update_available(&mut self, version: String) {
        self.update_available = Some(version);
        self.status_bar_update_rect = None;
        self.update_window_title();
        self.request_redraw();
    }

    fn handle_update_progress(&mut self, state: UpdateProgressState) {
        let restart_required =
            matches!(&state, UpdateProgressState::CompletedRestartRequired { .. });
        self.update_progress = Some(state);
        self.status_bar_update_rect = None;
        self.update_window_title();
        if restart_required {
            self.open_update_dialog();
        } else {
            self.request_redraw();
        }
    }

    fn handle_modal_window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        event: &WindowEvent,
    ) -> bool {
        if self.close_dialog.is_some() {
            self.handle_close_dialog_window_event(event_loop, event);
            return true;
        }
        if self.update_dialog.is_some() {
            self.handle_update_dialog_window_event(event_loop, event);
            return true;
        }
        false
    }

    fn handle_close_dialog_window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        event: &WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => self.handle_close_requested(event_loop),
            WindowEvent::RedrawRequested => self.handle_redraw(),
            WindowEvent::Resized(size) => self.handle_resize_and_mark_geometry(*size),
            WindowEvent::Moved(_) => self.mark_geometry_dirty(),
            WindowEvent::ModifiersChanged(new_mods) => self.modifiers = new_mods.state(),
            WindowEvent::KeyboardInput { event: key_event, .. } => {
                if self.handle_native_close_window_shortcut(key_event, event_loop) {
                    return;
                }
                self.handle_dialog_keyboard(key_event, event_loop);
            }
            WindowEvent::MouseInput {
                state: winit::event::ElementState::Pressed,
                button: winit::event::MouseButton::Left,
                ..
            } => self.handle_dialog_click(event_loop),
            WindowEvent::CursorMoved { position, .. } => {
                self.update_cursor_position(position);
                self.handle_dialog_hover();
            }
            _ => {}
        }
    }

    fn handle_update_dialog_window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        event: &WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => self.handle_close_requested(event_loop),
            WindowEvent::RedrawRequested => self.handle_redraw(),
            WindowEvent::Resized(size) => self.handle_resize_and_mark_geometry(*size),
            WindowEvent::Moved(_) => self.mark_geometry_dirty(),
            WindowEvent::ModifiersChanged(new_mods) => self.modifiers = new_mods.state(),
            WindowEvent::KeyboardInput { event: key_event, .. } => {
                if self.handle_native_close_window_shortcut(key_event, event_loop) {
                    return;
                }
                self.handle_update_dialog_keyboard(key_event);
            }
            WindowEvent::MouseInput {
                state: winit::event::ElementState::Pressed,
                button: winit::event::MouseButton::Left,
                ..
            } => self.handle_update_dialog_click(),
            WindowEvent::CursorMoved { position, .. } => {
                self.update_cursor_position(position);
                self.handle_update_dialog_hover();
            }
            _ => {}
        }
    }

    fn handle_main_window_event(&mut self, event_loop: &ActiveEventLoop, event: &WindowEvent) {
        match event {
            WindowEvent::CloseRequested => self.handle_close_requested(event_loop),
            WindowEvent::RedrawRequested => self.handle_redraw(),
            WindowEvent::Resized(size) => self.handle_resize_and_mark_geometry(*size),
            WindowEvent::Moved(_) => self.mark_geometry_dirty(),
            WindowEvent::ModifiersChanged(new_mods) => {
                self.modifiers = new_mods.state();
                if let Some((x, y)) = self.last_cursor_pos {
                    self.update_hover_cursor(x, y);
                }
                self.request_redraw();
            }
            WindowEvent::KeyboardInput { event: key_event, .. } => {
                if self.handle_native_close_window_shortcut(key_event, event_loop) {
                    return;
                }
                if self.focus.window_focused && !self.compositor_overlay_active() {
                    self.handle_keyboard(key_event);
                }
            }
            WindowEvent::DroppedFile(path) => self.handle_dropped_path(path),
            WindowEvent::MouseInput { state, button, .. } => {
                self.handle_mouse_input(*state, *button);
            }
            WindowEvent::MouseWheel { delta, .. } => self.handle_mouse_wheel(*delta),
            WindowEvent::CursorMoved { position, .. } => {
                self.update_cursor_position(position);
                self.handle_cursor_moved();
            }
            WindowEvent::Focused(focused) => self.handle_focus_changed(*focused),
            _ => {}
        }
    }

    fn handle_resize_and_mark_geometry(&mut self, size: winit::dpi::PhysicalSize<u32>) {
        self.handle_resize(size);
        self.mark_geometry_dirty();
    }

    fn update_cursor_position(&mut self, position: &winit::dpi::PhysicalPosition<f64>) {
        let position = position.cast::<f32>();
        self.last_cursor_pos = Some((position.x, position.y));
    }

    fn handle_focus_changed(&mut self, focused: bool) {
        self.focus.window_focused = focused;
        if focused && let Some(window) = &self.window {
            window.request_user_attention(None);
        }
        if focused {
            self.handle_focus_gained();
        } else {
            let session = self.focused_session_id();
            self.notify_focus_change(None, session);
        }
        self.request_redraw();
    }

    fn handle_focus_gained(&mut self) {
        #[cfg(target_os = "linux")]
        if let Some(guard) = &mut self.x11_focus_guard {
            guard.clear_reactivation_debounce();
        }

        if let Some(session_id) = self.notification_tracker.take_pending_focus() {
            self.handle_focus_session(session_id);
        }

        self.cursor.visible = true;
        self.blink_timer = Instant::now();
        let session = self.focused_session_id();
        self.notify_focus_change(session, None);
    }
}

// ---------------------------------------------------------------------------
// Initialisation
// ---------------------------------------------------------------------------

impl App {
    fn handle_native_close_window_shortcut(
        &mut self,
        event: &winit::event::KeyEvent,
        event_loop: &ActiveEventLoop,
    ) -> bool {
        if is_macos_close_window_shortcut(event, self.modifiers) {
            self.handle_close_requested(event_loop);
            true
        } else {
            false
        }
    }

    /// Initialise the window, wgpu surface/device/queue, renderer, layout,
    /// and IPC thread.
    fn init_gpu_and_terminal(&mut self, event_loop: &ActiveEventLoop) -> Result<(), InitError> {
        // Set a reasonable initial size so the GPU surface, renderer, and
        // pane grids have usable dimensions even before the compositor sends
        // a configure event.  On Wayland, inner_size() can return a tiny
        // default until the first configure; this hint prevents that.
        let window_title = current_identity().window_title_name();
        let mut attrs = Window::default_attributes()
            .with_title(window_title)
            .with_inner_size(winit::dpi::LogicalSize::new(1200.0, 800.0));
        if self.window_transparent {
            attrs = attrs.with_transparent(true);
            tracing::info!(opacity = self.opacity, "window transparency enabled");
        }
        let window = Arc::new(event_loop.create_window(attrs).map_err(InitError::Window)?);

        // Restore saved window geometry (position, size, maximized state).
        if let Some(geom) = self.saved_geometry.take() {
            let _ = window_state::apply_window_geometry(event_loop, &window, &geom);
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
        let scale = window.scale_factor().cast::<f32>();
        self.scale_factor = scale;
        let font_params = scribe_renderer::atlas::FontParams {
            family: self.config.appearance.font.clone(),
            size: self.config.appearance.font_size * scale,
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

        // Start the platform notification dispatcher on its own
        // thread — see lat.md/client.md §Desktop Notifications.
        if let Some(notif_proxy) = self.animation_proxy.clone() {
            self.notification_tx = Some(notification_dispatcher::spawn_dispatcher(notif_proxy));
        }

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

        // Initialise X11 active-window guard (Linux/X11 only).
        #[cfg(target_os = "linux")]
        if let Some(wid) = x11_window_id(&window) {
            self.x11_focus_guard = x11_focus::X11FocusGuard::new(wid);
            if self.x11_focus_guard.is_some() {
                tracing::debug!("X11 active-window guard initialised");
            }
        }

        self.window = Some(window);

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Event handlers
// ---------------------------------------------------------------------------

impl App {
    /// Apply a screen snapshot (from the `RequestSnapshot` tooling path) to a
    /// pane by converting it to ANSI escape sequences and feeding them through
    /// the normal VTE pipeline. Reattach uses [`handle_session_replay`] with
    /// pre-compressed ANSI from the server and shares [`apply_replay_ansi`].
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
            alt_screen = snapshot.alt_screen,
            "applying screen snapshot"
        );

        let ansi = snapshot_to_ansi(snapshot);
        self.apply_replay_ansi(session_id, snapshot.cols, snapshot.rows, &ansi);
    }

    /// Apply a `SessionReplay` from the server's reattach path. Decompresses
    /// the zstd'd ANSI stream and feeds it through [`apply_replay_ansi`].
    fn handle_session_replay(
        &mut self,
        session_id: SessionId,
        replay: &scribe_common::screen_replay::SessionReplay,
    ) {
        if replay.cols == 0 || replay.rows == 0 {
            tracing::warn!(%session_id, "SessionReplay has zero dimensions, skipping");
            return;
        }

        let ansi = match scribe_common::screen_replay::decompress_session_replay(replay) {
            Ok(bytes) => bytes,
            Err(error) => {
                tracing::warn!(%session_id, %error, "failed to decompress SessionReplay");
                return;
            }
        };
        tracing::info!(
            %session_id,
            cols = replay.cols,
            rows = replay.rows,
            scrollback_rows = replay.scrollback_rows,
            alt_screen = replay.alt_screen,
            compressed = replay.replay_zstd.len(),
            ansi_len = ansi.len(),
            "applying SessionReplay"
        );

        self.apply_replay_ansi(session_id, replay.cols, replay.rows, &ansi);
    }

    /// Shared body that feeds ANSI bytes into a pane's Term, handling the
    /// server-vs-client grid mismatch and scheduling a post-restore resize.
    fn apply_replay_ansi(&mut self, session_id: SessionId, cols: u16, rows: u16, ansi: &[u8]) {
        let Some(pane_id) = self.session_to_pane.get(&session_id).copied() else {
            tracing::warn!(%session_id, "replay: no pane for session");
            return;
        };
        let is_codex = self
            .ai_tracker
            .provider_for_session(session_id)
            .is_some_and(|p| p == AiProvider::CodexCode);
        let needs_post_restore_resize = {
            let Some(pane) = self.panes.get_mut(&pane_id) else {
                tracing::warn!(%session_id, "replay: pane not found");
                return;
            };

            // The replay was captured with the server's current term
            // dimensions, which may still differ from the restored pane grid.
            // Feed the ANSI through a term sized to the replay, then resize
            // back to the pane.
            let pane_grid = pane.grid;
            let dims_match = pane_grid.cols == cols && pane_grid.rows == rows;

            if !dims_match {
                tracing::info!(
                    replay_cols = cols,
                    replay_rows = rows,
                    pane_cols = pane_grid.cols,
                    pane_rows = pane_grid.rows,
                    "replay dimensions differ from pane — resizing term temporarily"
                );
                pane.resize_term_only(cols, rows);
            }

            let _ = pane.feed_output(ansi);

            if !dims_match {
                pane.resize_term_only(pane_grid.cols, pane_grid.rows);

                // Codex (via Ink) uses cursor-addressed rendering and may not
                // fully clear the screen on SIGWINCH redraw. The reflow from
                // resizing the replay content to the pane's grid garbles the
                // TUI layout, and Ink's differential render leaves remnants of
                // the old replay visible. Clear the visible area so Codex's
                // SIGWINCH redraw starts from a clean slate — scrollback from
                // the replay is preserved.
                clear_restored_codex_snapshot_artifacts(pane, is_codex);

                // A mismatched replay means the server-side PTY was not
                // actually restored at this pane's grid yet, regardless of
                // what we assumed before AttachSessions.
                pane.last_sent_grid = None;
            }

            !dims_match
        };

        if needs_post_restore_resize {
            self.resize_pending = Some(Instant::now());
        }

        // Mark content as ready so the splash can be dismissed once it has
        // been visible for MIN_SPLASH_DURATION. The actual dismissal happens
        // in `handle_redraw` to avoid submitting the terminal-content frame
        // before the compositor has presented the splash frame.
        if self.splash.active {
            self.splash.content_ready = true;
        }

        self.request_redraw();
    }

    /// Drain all pending PTY output buffers into per-pane committed frames.
    ///
    /// IPC delivery is still coalesced per session so input events are never
    /// blocked behind a burst of `PtyOutput` messages, but redraw pacing is
    /// decided from pane-local frame queues to preserve synchronized-update
    /// commit boundaries.
    fn drain_pending_pty_output(&mut self) {
        if self.pending_pty_bytes.is_empty() {
            return;
        }
        let pending = std::mem::take(&mut self.pending_pty_bytes);
        for (session_id, data) in pending {
            self.handle_pty_output(session_id, &data);
        }
    }

    fn handle_pty_output(&mut self, session_id: SessionId, bytes: &[u8]) {
        let Some(pane_id) = self.session_to_pane.get(&session_id).copied() else { return };
        self.enqueue_pane_output_frames(pane_id, bytes);
    }

    fn enqueue_pane_output_frames(&mut self, pane_id: PaneId, bytes: &[u8]) {
        let queue_was_empty =
            self.panes.get(&pane_id).is_some_and(|pane| pane.pending_output_frames.is_empty());
        let queued_any = {
            let Some(pane) = self.panes.get_mut(&pane_id) else { return };
            pane.queue_output_frames(bytes)
        };

        if !queued_any {
            if self.panes.get(&pane_id).is_some_and(Pane::has_pending_sync_update) {
                self.ensure_animation_running();
            }
            return;
        }

        // When the pane was idle, advance to the first committed frame
        // immediately so the next render reflects fresh content instead of
        // waiting for another event-loop turn.
        if queue_was_empty && let Some(result) = self.drain_pane_output_until_frame(pane_id) {
            if result.sync_pending {
                self.ensure_animation_running();
            }
            if result.needs_redraw || result.queue_state.has_more() {
                self.request_redraw();
            }
            return;
        }

        self.request_redraw();
    }

    fn drain_pending_output_frames(&mut self) -> bool {
        let pane_ids: Vec<PaneId> = self
            .panes
            .iter()
            .filter_map(|(&pane_id, pane)| {
                (!pane.pending_output_frames.is_empty()).then_some(pane_id)
            })
            .collect();
        let mut request_redraw = false;

        for pane_id in pane_ids {
            let Some(result) = self.drain_pane_output_until_frame(pane_id) else { continue };
            if result.sync_pending {
                self.ensure_animation_running();
            }
            request_redraw |= result.queue_state.has_more();
        }

        request_redraw
    }

    fn drain_pane_output_until_frame(
        &mut self,
        pane_id: PaneId,
    ) -> Option<PendingOutputDrainResult> {
        let mut sync_pending = false;
        let catch_up_to_latest = self
            .panes
            .get(&pane_id)
            .is_some_and(|pane| pane.pending_output_frames.len() > OUTPUT_FRAME_CATCH_UP_THRESHOLD);

        loop {
            let bytes = {
                let pane = self.panes.get_mut(&pane_id)?;
                pane.pending_output_frames.pop_front()?
            };
            let feed = self.apply_pane_output_bytes(pane_id, &bytes)?;
            let remaining_frames =
                self.panes.get(&pane_id).map_or(0, |pane| pane.pending_output_frames.len());
            let has_more = remaining_frames != 0;
            sync_pending |= feed.sync_pending;
            let keep_draining = catch_up_to_latest && has_more;

            if !keep_draining && (feed.needs_redraw || !has_more) {
                return Some(PendingOutputDrainResult {
                    needs_redraw: feed.needs_redraw,
                    queue_state: DrainQueueState::from_has_more(has_more),
                    sync_pending,
                });
            }
        }
    }

    fn apply_pane_output_bytes(
        &mut self,
        pane_id: PaneId,
        bytes: &[u8],
    ) -> Option<FeedOutputResult> {
        let session_id = self.panes.get(&pane_id)?.session_id;
        let split_scroll_eligibility = SplitScrollEligibility::for_session(
            session_id,
            &self.ai_tracker,
            &self.config.terminal,
        );
        let (feed, delta, topmost) = {
            let pane = self.panes.get_mut(&pane_id)?;
            let old_history = pane.term.grid().history_size();
            let feed = pane.feed_output(bytes);
            reconcile_split_scroll(pane, split_scroll_eligibility);
            let new_history = pane.term.grid().history_size();
            let delta = history_size_delta(new_history, old_history);
            let topmost = pane.term.grid().topmost_line().0;
            (feed, delta, topmost)
        };

        if feed.needs_redraw {
            self.note_pane_content_change(pane_id, delta, topmost);
        }

        Some(feed)
    }

    /// Apply shared post-processing after a pane's terminal content changes.
    fn note_pane_content_change(&mut self, pane_id: PaneId, delta: i32, topmost: i32) {
        // Invalidate the URL cache so it re-scans on next hover check.
        if let Some(cache) = self.url_caches.get_mut(&pane_id) {
            cache.mark_dirty();
        }

        // Standard terminal behavior: visible output clears the active
        // selection in the focused pane unless the user is actively dragging.
        let focused_pane = self.window_layout.active_tab().map(|t| t.focused_pane);
        if focused_pane == Some(pane_id) && !self.mouse_selecting {
            self.active_selection = None;
            self.word_drag_anchor = None;
            self.line_drag_anchor = None;
        }

        // Adjust selections to follow scrollback growth.
        if delta != 0 {
            if focused_pane == Some(pane_id) {
                self.shift_active_selection(delta, topmost);
            } else {
                self.shift_background_tab_selection(pane_id, delta, topmost);
            }
        }

        // Mark content as ready (same deferred-dismiss as screen snapshots).
        if self.splash.active {
            self.splash.content_ready = true;
        }
    }

    /// Flush synchronized updates whose VTE timeout has elapsed.
    ///
    /// Returns `(sync_pending, flushed_any)` after scanning all panes.
    fn flush_expired_sync_updates(&mut self, now: Instant) -> (bool, bool) {
        let mut sync_pending = false;
        let mut flushed: Vec<(PaneId, i32, i32)> = Vec::new();

        for (&pane_id, pane) in &mut self.panes {
            let old_history = pane.term.grid().history_size();
            if pane.flush_sync_timeout(now) {
                let new_history = pane.term.grid().history_size();
                let delta = history_size_delta(new_history, old_history);
                let topmost = pane.term.grid().topmost_line().0;
                flushed.push((pane_id, delta, topmost));
            }
            sync_pending |= pane.has_pending_sync_update();
        }

        let flushed_any = !flushed.is_empty();
        for (pane_id, delta, topmost) in flushed {
            self.note_pane_content_change(pane_id, delta, topmost);
        }

        (sync_pending, flushed_any)
    }

    /// Shift the active selection and drag anchors by `delta` rows and clear
    /// them if the selection start moves above `topmost`.
    fn shift_active_selection(&mut self, delta: i32, topmost: i32) {
        if let Some(sel) = &mut self.active_selection {
            sel.shift_rows(-delta);
        }
        if let Some((ref mut start, ref mut end)) = self.word_drag_anchor {
            start.shift_row(-delta);
            end.shift_row(-delta);
        }
        if let Some((ref mut start, ref mut end)) = self.line_drag_anchor {
            start.shift_row(-delta);
            end.shift_row(-delta);
        }
        if self.active_selection.is_some_and(|s| s.normalized().0.row < topmost) {
            self.active_selection = None;
            self.word_drag_anchor = None;
            self.line_drag_anchor = None;
        }
    }

    /// Shift the saved selection for any background tab whose focused pane
    /// matches `pane_id`, and clear the selection if it moves above `topmost`.
    fn shift_background_tab_selection(&mut self, pane_id: PaneId, delta: i32, topmost: i32) {
        let ws_ids = self.window_layout.workspace_ids_in_order();
        for ws_id in ws_ids {
            let Some(ws) = self.window_layout.find_workspace_mut(ws_id) else { continue };
            for tab in &mut ws.tabs {
                shift_tab_selection_for_pane(tab, pane_id, delta, topmost);
            }
        }
    }

    /// Send `ListSessions` once after the first splash frame renders.
    ///
    /// On a local Unix socket, the full IPC round-trip (`ListSessions` →
    /// `SessionList` → `AttachSessions` → `ScreenSnapshot`) completes in under
    /// 1 ms, while the compositor's first frame callback takes ~16 ms.
    /// Deferring this send until after the splash is on-screen prevents the
    /// session content from arriving before the splash has been displayed.
    fn send_deferred_list_sessions(&mut self) {
        if !self.splash_load.needs_list_sessions {
            return;
        }
        self.splash_load.needs_list_sessions = false;
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
        event_loop: &ActiveEventLoop,
        sessions: &[scribe_common::protocol::SessionInfo],
        workspace_tree: Option<&scribe_common::protocol::WorkspaceTreeNode>,
        workspaces: &[scribe_common::protocol::WorkspaceListEntry],
    ) {
        let Some(tx) = self.cmd_tx.clone() else { return };

        self.connection.server_connected = true;
        self.legacy_workspace_direction_updates.clear();
        if sessions.is_empty() {
            self.try_cold_restart_or_fresh(event_loop);
            return;
        }

        tracing::info!(count = sessions.len(), "reattaching to existing sessions");

        // Apply per-workspace metadata (names, accent colors, project roots)
        // up front so the reconstructed layout carries correct styling before
        // the first pane is drawn. Replaces the legacy per-session
        // `WorkspaceInfo` fan-out that used to follow every attach.
        for entry in workspaces {
            self.handle_workspace_info(WorkspaceInfoUpdate {
                workspace_id: entry.workspace_id,
                name: entry.name.clone(),
                accent_color: entry.accent_color.clone(),
                split_direction: entry.split_direction,
                project_root: entry.project_root.clone(),
            });
        }

        let attach_ids = collect_session_ids(sessions);
        let metadata = build_session_metadata_map(sessions);
        let groups = group_sessions_by_workspace(sessions);
        let live_workspace_ids: HashSet<WorkspaceId> = groups.keys().copied().collect();
        let tab_pane_trees = workspace_tree.map_or_else(HashMap::new, extract_tab_pane_trees);

        let tabs_by_ws = self.reconstruct_workspaces_for_sessions(
            sessions,
            workspace_tree,
            &groups,
            &live_workspace_ids,
        );
        let ws_rects_map = self.compute_ws_dim_maps().1;
        let Some(fallback_viewport) = self.fallback_workspace_viewport() else { return };
        let reconnect_context = ReconnectRestoreContext {
            ws_rects_map: &ws_rects_map,
            fallback_viewport,
            tab_pane_trees: &tab_pane_trees,
            metadata: &metadata,
        };
        self.restore_reconnect_tabs(&tabs_by_ws, &reconnect_context);
        self.restore_initial_session_state(sessions);

        let (attach_dims, codex_sessions) = self.build_attach_request(sessions);
        send_command(
            &tx,
            ClientCommand::AttachSessions { session_ids: attach_ids, dimensions: attach_dims },
        );
        self.mark_reconnected_grids(&codex_sessions);
        send_command(&tx, ClientCommand::Subscribe { session_ids: collect_session_ids(sessions) });
        self.request_redraw();
    }

    fn reconstruct_workspaces_for_sessions(
        &mut self,
        sessions: &[scribe_common::protocol::SessionInfo],
        workspace_tree: Option<&scribe_common::protocol::WorkspaceTreeNode>,
        groups: &HashMap<WorkspaceId, Vec<SessionId>>,
        live_workspace_ids: &HashSet<WorkspaceId>,
    ) -> Vec<(WorkspaceId, Vec<SessionId>)> {
        if let Some(tree) = workspace_tree {
            self.reconstruct_from_tree(tree, live_workspace_ids);
        } else {
            self.reconstruct_fallback(sessions);
        }

        let tabs_by_ws = ordered_workspace_tabs(&self.window_layout, groups);
        if tabs_by_ws.is_empty() && !groups.is_empty() {
            tracing::warn!("workspace tree IDs do not match session workspace IDs — falling back");
            self.reconstruct_fallback(sessions);
            return ordered_workspace_tabs(&self.window_layout, groups);
        }

        tabs_by_ws
    }

    fn fallback_workspace_viewport(&self) -> Option<Rect> {
        let gpu = self.gpu.as_ref()?;
        Some(workspace_viewport(
            &gpu.surface_config,
            self.config.appearance.status_bar_height * self.scale_factor,
        ))
    }

    fn restore_reconnect_tabs(
        &mut self,
        tabs_by_ws: &[(WorkspaceId, Vec<SessionId>)],
        context: &ReconnectRestoreContext<'_>,
    ) {
        let tab_restore_list =
            build_tab_restore_list(tabs_by_ws, context.ws_rects_map, context.fallback_viewport);
        for (ws_id, sid, ws_rect, tab_count) in tab_restore_list {
            if self.session_to_pane.contains_key(&sid) {
                continue;
            }
            self.restore_tab_panes(TabRestoreRequest {
                workspace_id: ws_id,
                session_id: sid,
                workspace_rect: ws_rect,
                tab_count,
                pane_tree: context.tab_pane_trees.get(&sid),
                metadata: context.metadata,
            });
        }
    }

    fn restore_initial_session_state(&mut self, sessions: &[scribe_common::protocol::SessionInfo]) {
        for info in sessions {
            if let Some(provider) = info.ai_provider_hint {
                self.ai_tracker.remember_provider(info.session_id, provider);
            }
            if let Some(state) = info.ai_state.clone() {
                tracing::info!(
                    session_id = %info.session_id,
                    ai_state = ?state,
                    "restoring AI state from initial session list"
                );
                self.handle_ai_state_changed(info.session_id, state);
            }
            // Apply git branch directly from SessionInfo. Before the metadata
            // fan-out was removed this was redelivered as `GitBranch`; the
            // branch now rides on `SessionInfo` itself so the attach pipeline
            // only needs to send `SessionCreated` + `SessionReplay`.
            if info.git_branch.is_some() {
                self.handle_git_branch(info.session_id, info.git_branch.clone());
            }
        }
        self.apply_snapshot_prompt_state();
    }

    fn build_attach_request(
        &self,
        sessions: &[scribe_common::protocol::SessionInfo],
    ) -> (Vec<TerminalSize>, HashSet<SessionId>) {
        let codex_sessions: HashSet<SessionId> = sessions
            .iter()
            .filter_map(|info| is_codex_session(info).then_some(info.session_id))
            .collect();
        let attach_dims = sessions
            .iter()
            .map(|info| {
                if codex_sessions.contains(&info.session_id) {
                    TerminalSize::default()
                } else {
                    self.session_to_pane
                        .get(&info.session_id)
                        .and_then(|pid| self.panes.get(pid))
                        .and_then(|pane| self.terminal_size_for_grid(pane.grid))
                        .unwrap_or_default()
                }
            })
            .collect();
        (attach_dims, codex_sessions)
    }

    fn mark_reconnected_grids(&mut self, codex_sessions: &HashSet<SessionId>) {
        for pane in self.panes.values_mut() {
            pane.last_sent_grid =
                if codex_sessions.contains(&pane.session_id) { None } else { Some(pane.grid) };
        }
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
        self.legacy_workspace_direction_updates.clear();

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

        // Old servers do not send a workspace tree, so the linear fallback
        // needs one pass of per-workspace direction patches from the
        // subsequent WorkspaceInfo messages. After each workspace has been
        // patched once, later WorkspaceInfo events must not rewrite the live
        // split topology.
        self.legacy_workspace_direction_updates = workspace_order.into_iter().collect();
    }

    /// Restore a single tab's panes during reconnect, using a pane tree if available.
    ///
    /// When `pane_tree` is `Some`, the full split layout is restored via
    /// `add_tab_with_pane_tree`.  Otherwise a single-pane tab is created via
    /// `add_tab`.  All resulting `(SessionId, PaneId)` pairs are registered in
    /// `self.panes` and `self.session_to_pane`.
    fn restore_tab_panes(&mut self, request: TabRestoreRequest<'_>) {
        let Some(gpu) = self.gpu.as_ref() else { return };
        let cell = gpu.renderer.cell_size();

        let pairs = if let Some(tree) = request.pane_tree {
            self.window_layout
                .add_tab_with_pane_tree(request.workspace_id, request.session_id, tree)
                .unwrap_or_default()
        } else {
            let Some(pane_id) =
                self.window_layout.add_tab(request.workspace_id, request.session_id)
            else {
                return;
            };
            vec![(request.session_id, pane_id)]
        };

        let pane_rects = self
            .window_layout
            .find_workspace(request.workspace_id)
            .and_then(|ws| ws.active_tab())
            .map(|tab| tab.pane_layout.compute_rects(request.workspace_rect))
            .unwrap_or_default();
        let tab_bar_h = self.tab_bar_height_for_tab_count(
            request.workspace_id,
            request.workspace_rect,
            request.tab_count,
        );

        for (pane_sid, pane_id) in pairs {
            let (pane_rect, pane_edges) =
                pane_rects.iter().find(|(id, _, _)| *id == pane_id).map_or(
                    (request.workspace_rect, PaneEdges::all_external()),
                    |&(_, rect, edges)| (rect, edges),
                );
            let eff_tbh = if pane_edges.top() { tab_bar_h } else { 0.0 };
            let grid = pane::compute_pane_grid(&pane::PaneGridRequest {
                rect: pane_rect,
                cell_size: (cell.width, cell.height),
                tab_bar_height: eff_tbh,
                prompt_bar_height: 0.0,
                padding: &pane::effective_padding(
                    &self.config.appearance.content_padding,
                    pane_edges,
                    self.scale_factor,
                ),
            });
            let binding = if let Some((_, _, cwd, _, _, Some(provider), conversation_id)) =
                request.metadata.get(&pane_sid)
            {
                restore_replay::new_ai_binding(
                    *provider,
                    restore_state::AiResumeMode::Resume,
                    cwd.cloned(),
                    conversation_id.map(ToOwned::to_owned),
                )
            } else {
                restore_replay::new_shell_binding(
                    request
                        .metadata
                        .get(&pane_sid)
                        .and_then(|(_, _, cwd, _, _, _, _)| cwd.cloned()),
                )
            };
            let mut pane = Pane::new(
                pane::PaneLayoutState { rect: pane_rect, grid, edges: pane_edges },
                pane_sid,
                request.workspace_id,
                binding,
            );
            apply_session_metadata(&mut pane, request.metadata);
            self.panes.insert(pane_id, pane);
            self.url_caches.insert(pane_id, url_detect::PaneUrlCache::new());
            self.session_to_pane.insert(pane_sid, pane_id);
        }
    }

    /// Create the initial session + pane for a fresh start (no existing sessions).
    fn create_initial_session(&mut self) {
        let Some(tx) = &self.cmd_tx else { return };

        let workspace_id = self.window_layout.focused_workspace_id();
        let session_id = SessionId::new();

        let Some(pane_id) = self.window_layout.add_tab(workspace_id, session_id) else { return };

        let Some((_first_id, pane_rect, grid)) = self.first_pane_geometry() else { return };
        let pane = Pane::new(
            pane::PaneLayoutState { rect: pane_rect, grid, edges: PaneEdges::all_external() },
            session_id,
            workspace_id,
            restore_state::LaunchBinding {
                launch_id: SessionId::new().to_full_string(),
                kind: restore_state::LaunchKind::Shell,
                fallback_cwd: None,
            },
        );

        send_command(
            tx,
            ClientCommand::CreateSession {
                workspace_id,
                split_direction: None,
                cwd: None,
                size: self.terminal_size_for_grid(grid),
                command: None,
            },
        );
        self.panes.insert(pane_id, pane);
        self.url_caches.insert(pane_id, url_detect::PaneUrlCache::new());
        self.session_to_pane.insert(session_id, pane_id);
        self.pending_sessions.push_back(session_id);

        // Seed the server with the initial (single-leaf) tree.
        self.report_workspace_tree();
    }

    /// Try to restore the previous layout from a cold restart snapshot.
    /// Falls back to a fresh session if no snapshot is available or replay
    /// fails.
    fn try_cold_restart_or_fresh(&mut self, event_loop: &ActiveEventLoop) {
        // Only attempt cold restart restore when launched without --window-id
        // (i.e. a fresh launch after a server crash).  Windows spawned via
        // handle_new_window() always carry a pre-assigned window_id and must
        // start with a blank session — otherwise they would claim another
        // live window's restore snapshot and appear as a duplicate.
        if self.launch.explicit_new_window {
            self.create_initial_session();
            return;
        }
        let claimed = self.restore_store.claim_first_window();
        let restored = claimed.as_ref().is_some_and(|(snapshot, _)| {
            tracing::info!(
                window_id = %snapshot.window_id,
                "restoring window layout from cold restart snapshot"
            );
            // True cold restart connects to a fresh server, so `Welcome`
            // assigns a new window ID before we learn which snapshot we
            // claimed. Geometry is keyed by the pre-crash snapshot ID.
            //
            // Capture the applied geom so the replay can size pane grids
            // from it directly: `request_inner_size` and `set_maximized`
            // are async on most compositors, so `window.inner_size()`
            // would still report the pre-restore initial hint here.
            let saved_geom = self.restore_geometry_from_registry(event_loop, snapshot.window_id);
            self.replay_cold_restart(snapshot, saved_geom.as_ref())
        });
        if restored {
            let remaining = claimed.map_or(0, |(_, r)| r);
            if !self.launch.restore_spawn_child {
                (0..remaining).for_each(|_| spawn_fresh_client_process());
            }
        } else {
            self.create_initial_session();
        }
    }

    /// Rebuild the window layout from a cold restart snapshot and create
    /// sessions for each saved pane.
    ///
    /// `saved_geom` is the geometry that was just reapplied via
    /// `apply_window_geometry` for `snapshot.window_id`, when one exists.
    /// Sizing pane grids from it (instead of `window.inner_size()`) avoids
    /// a race where the compositor has not yet acknowledged the
    /// `request_inner_size`/`set_maximized` calls made microseconds earlier
    /// — the previous code path created PTYs at the 1200×800 startup hint
    /// for any maximized window, leaving the cells stuck below the actual
    /// viewport for the lifetime of the session because the corrective
    /// `Resize` IPC is dispatched while panes still hold placeholder
    /// session IDs that the server cannot match.
    fn replay_cold_restart(
        &mut self,
        snapshot: &restore_state::WindowRestoreState,
        saved_geom: Option<&window_state::WindowGeometry>,
    ) -> bool {
        let Some(tx) = self.cmd_tx.clone() else { return false };
        let Some(gpu) = self.gpu.as_ref() else { return false };
        let Some(window) = self.window.as_ref() else { return false };
        if !snapshot.is_replayable() {
            tracing::warn!(
                window_id = %snapshot.window_id,
                launches = snapshot.launches.len(),
                "skipping non-replayable cold restart snapshot"
            );
            return false;
        }

        let mut replay =
            restore_replay::prepare_replay(snapshot, &mut self.window_layout, &mut self.panes);

        // Prefer the saved geom's logical dimensions (× current
        // scale_factor) over `window.inner_size()`. Both `request_inner_size`
        // and `set_maximized(true)` are async on most compositors, so
        // `inner_size()` here typically still returns the pre-restore
        // initial 1200×800 hint — sizing pane grids from that would leave
        // the PTY undersized once the compositor settles on the real
        // (often-maximized) viewport.
        let observed_inner = window.inner_size();
        let win_size = saved_geom.map_or(observed_inner, |geom| {
            window_state::expected_physical_size(geom, self.scale_factor)
        });
        let surface_w = gpu.surface_config.width;
        let surface_h = gpu.surface_config.height;
        tracing::info!(
            win_w = win_size.width,
            win_h = win_size.height,
            inner_w = observed_inner.width,
            inner_h = observed_inner.height,
            surface_w,
            surface_h,
            used_saved_geom = saved_geom.is_some(),
            "replay_cold_restart: window and surface dimensions"
        );
        let win_size = win_size.cast::<f32>();
        let viewport = Rect {
            x: 0.0,
            y: 0.0,
            width: win_size.width,
            height: (win_size.height
                - self.config.appearance.status_bar_height * self.scale_factor)
                .max(1.0),
        };
        let cell = gpu.renderer.cell_size();
        self.recompute_replay_pane_geometry(viewport, cell);

        // Drive the replay queue: create a session for each saved pane.
        while let Some(launch) = restore_replay::next_launch(&mut replay) {
            self.session_to_pane.insert(launch.placeholder_session_id, launch.pane_id);
            self.url_caches.insert(launch.pane_id, url_detect::PaneUrlCache::new());
            self.pending_sessions.push_back(launch.placeholder_session_id);

            let term_size = self
                .panes
                .get(&launch.pane_id)
                .and_then(|pane| self.terminal_size_for_grid(pane.grid));

            send_command(
                &tx,
                ClientCommand::CreateSession {
                    workspace_id: launch.workspace_id,
                    split_direction: None,
                    cwd: launch.cwd.clone(),
                    size: term_size,
                    command: restore_replay::command_argv(&launch.command),
                },
            );
        }

        self.report_workspace_tree();
        true
    }

    /// Return the configured single-row tab bar height.
    fn effective_tab_bar_height(&self) -> f32 {
        (self.config.appearance.tab_height + self.config.appearance.tab_bar_padding)
            * self.scale_factor
    }

    /// Compute the tab bar height for a specific workspace, accounting for
    /// multi-row stacking based on tab count and workspace width.
    fn tab_bar_height_for(&self, workspace_id: WorkspaceId, ws_rect: Rect) -> f32 {
        let tab_count =
            self.window_layout.find_workspace(workspace_id).map_or(1, |ws| ws.tabs.len().max(1));
        self.tab_bar_height_for_tab_count(workspace_id, ws_rect, tab_count)
    }

    fn tab_bar_height_for_tab_count(
        &self,
        workspace_id: WorkspaceId,
        ws_rect: Rect,
        tab_count: usize,
    ) -> f32 {
        let cell_w = self.gpu.as_ref().map_or(8.0, |g| g.renderer.cell_size().width);
        let row_h = self.config.appearance.tab_height * self.scale_factor;
        let workspace = self.window_layout.find_workspace(workspace_id);
        let badge_cols = tab_bar::badge_columns(
            workspace.and_then(|ws| ws.name.as_deref()),
            self.window_layout.workspace_count() > 1,
        );
        let trailing_controls = tab_bar::TabBarTrailingControls {
            gear: false,
            equalize: workspace
                .and_then(|ws| ws.active_tab())
                .is_some_and(|tab| tab.pane_layout.all_pane_ids().len() > 1),
        };
        tab_bar::compute_tab_bar_height(tab_bar::TabBarHeightRequest {
            tab_count: tab_count.max(1),
            ws_width: ws_rect.width,
            tab_width_chars: self.config.appearance.tab_width,
            cell_w,
            row_height: row_h,
            badge_cols,
            trailing_controls,
        })
    }

    /// Return the rects for panes that are currently visible in active tabs.
    ///
    /// Hidden tabs keep their last `pane.rect`, so interaction hit-testing must
    /// derive visibility from the live layout instead of iterating `self.panes`.
    fn visible_pane_rects(&self) -> Vec<(PaneId, Rect, PaneEdges)> {
        let Some(gpu) = &self.gpu else { return Vec::new() };
        let ws_viewport = workspace_viewport(
            &gpu.surface_config,
            self.config.appearance.status_bar_height * self.scale_factor,
        );
        let ws_rects = self.window_layout.compute_workspace_rects(ws_viewport);
        collect_expected_pane_rects(&self.window_layout, &ws_rects)
    }

    /// Compute the on-screen prompt bar rect for a visible pane.
    fn prompt_bar_rect_for_visible_pane(
        &self,
        pane: &Pane,
        visible_pane: (Rect, PaneEdges),
        pb_cell_h: f32,
        pb_at_top: bool,
    ) -> Option<Rect> {
        let (pane_rect, pane_edges) = visible_pane;
        let pbh = pane.prompt_bar_height(pb_cell_h, true);
        if pbh <= 0.0 {
            return None;
        }
        let tbh = if pane_edges.top() {
            self.tab_bar_height_for(pane.workspace_id, pane_rect)
        } else {
            0.0
        };
        let bar_y =
            if pb_at_top { pane_rect.y + tbh } else { pane_rect.y + pane_rect.height - pbh };
        Some(Rect { x: pane_rect.x, y: bar_y, width: pane_rect.width, height: pbh })
    }

    /// Resolve the prompt-bar target under the cursor for the visible pane layout.
    fn prompt_bar_target_at(&self, x: f32, y: f32) -> Option<(PaneId, prompt_bar::PromptBarHover)> {
        if !self.config.terminal.prompt_bar.enabled {
            return None;
        }
        let Some(gpu) = &self.gpu else { return None };

        let cell = gpu.renderer.cell_size();
        let pb_font_scale =
            self.config.terminal.prompt_bar.font_size / self.config.appearance.font_size;
        let pb_cell = (cell.width * pb_font_scale, cell.height * pb_font_scale);
        let pb_at_top = self.config.terminal.prompt_bar.position
            == scribe_common::config::PromptBarPosition::Top;

        self.visible_pane_rects().into_iter().find_map(|(pane_id, pane_rect, pane_edges)| {
            let pane = self.panes.get(&pane_id)?;
            if pane.prompt_count == 0 || pane.prompt_ui.dismissed {
                return None;
            }
            let bar_rect = self.prompt_bar_rect_for_visible_pane(
                pane,
                (pane_rect, pane_edges),
                pb_cell.1,
                pb_at_top,
            )?;
            let hover = prompt_bar::hit_test_prompt_bar(pane, bar_rect, pb_cell, x, y)?;
            Some((pane_id, hover))
        })
    }

    /// Update geometry (rect, grid, Term dimensions) on every pane in the
    /// restored layout so they match the current window viewport.
    fn recompute_replay_pane_geometry(
        &mut self,
        viewport: Rect,
        cell: scribe_renderer::types::CellSize,
    ) {
        // Collect (pane_id, rect, edges, tab_bar_h) to avoid borrowing the
        // layout while mutating panes.
        let ws_rects = self.window_layout.compute_workspace_rects(viewport);
        let pane_geom: Vec<_> = ws_rects
            .iter()
            .filter_map(|&(ws_id, ws_rect)| {
                let workspace = self.window_layout.find_workspace(ws_id)?;
                let tab_count = workspace.tabs.len().max(1);
                let tbh = self.tab_bar_height_for_tab_count(ws_id, ws_rect, tab_count);
                Some(workspace.tabs.iter().flat_map(move |tab| {
                    tab.pane_layout
                        .compute_rects(ws_rect)
                        .into_iter()
                        .map(move |(pid, rect, edges)| (pid, rect, edges, tbh))
                }))
            })
            .flatten()
            .collect();

        for (pane_id, pane_rect, pane_edges, tab_bar_h) in pane_geom {
            let Some(pane) = self.panes.get_mut(&pane_id) else { continue };
            let eff_tbh = if pane_edges.top() { tab_bar_h } else { 0.0 };
            let pb_font_scale =
                self.config.terminal.prompt_bar.font_size / self.config.appearance.font_size;
            let pb_cell_h = cell.height * pb_font_scale;
            let pbh = pane.prompt_bar_height(pb_cell_h, self.config.terminal.prompt_bar.enabled);
            let grid = pane::compute_pane_grid(&pane::PaneGridRequest {
                rect: pane_rect,
                cell_size: (cell.width, cell.height),
                tab_bar_height: eff_tbh,
                prompt_bar_height: pbh,
                padding: &pane::effective_padding(
                    &self.config.appearance.content_padding,
                    pane_edges,
                    self.scale_factor,
                ),
            });
            pane.rect = pane_rect;
            pane.grid = grid;
            pane.edges = pane_edges;
            pane.resize_term_only(grid.cols, grid.rows);
        }
    }

    /// Compute the tab bar height for the currently focused workspace.
    ///
    /// Used by scrollbar and selection hit-testing where only the focused
    /// pane is relevant.
    fn focused_workspace_tab_bar_height(&self) -> f32 {
        let Some(gpu) = &self.gpu else { return self.effective_tab_bar_height() };
        let ws_viewport = workspace_viewport(
            &gpu.surface_config,
            self.config.appearance.status_bar_height * self.scale_factor,
        );
        let ws_id = self.window_layout.focused_workspace_id();
        let ws_rects = self.window_layout.compute_workspace_rects(ws_viewport);
        let ws_rect =
            ws_rects.iter().find(|(wid, _)| *wid == ws_id).map_or(ws_viewport, |(_, r)| *r);
        self.tab_bar_height_for(ws_id, ws_rect)
    }

    /// Compute per-workspace dimension, rect, and grid maps.
    ///
    /// Returns three `HashMap`s keyed by `WorkspaceId`:
    /// - `(cols, rows)` for IPC resize messages
    /// - `Rect` for pane creation
    /// - `GridSize` for pane creation
    ///
    /// All maps are empty when no GPU is present.
    fn compute_ws_dim_maps(&self) -> WorkspaceDimensionMaps {
        let Some(gpu) = self.gpu.as_ref() else {
            return (HashMap::new(), HashMap::new(), HashMap::new());
        };
        let ws_viewport = workspace_viewport(
            &gpu.surface_config,
            self.config.appearance.status_bar_height * self.scale_factor,
        );
        let ws_rects = self.window_layout.compute_workspace_rects(ws_viewport);
        let cell = gpu.renderer.cell_size();
        let tab_bar_h = self.effective_tab_bar_height();
        let padding = pane::effective_padding(
            &self.config.appearance.content_padding,
            PaneEdges::all_external(),
            self.scale_factor,
        );
        let mut dims = HashMap::new();
        let mut rects = HashMap::new();
        let mut grids = HashMap::new();
        for &(ws_id, ws_rect) in &ws_rects {
            let grid = pane::compute_pane_grid(&pane::PaneGridRequest {
                rect: ws_rect,
                cell_size: (cell.width, cell.height),
                tab_bar_height: tab_bar_h,
                prompt_bar_height: 0.0,
                padding: &padding,
            });
            dims.insert(ws_id, (grid.cols, grid.rows));
            rects.insert(ws_id, ws_rect);
            grids.insert(ws_id, grid);
        }
        (dims, rects, grids)
    }

    /// Compute the pane ID, rect, and grid size for the first pane of the
    /// active tab. Returns `None` if GPU or layout state is unavailable.
    fn first_pane_geometry(&self) -> Option<(PaneId, Rect, GridSize)> {
        let gpu = self.gpu.as_ref()?;
        let window = self.window.as_ref()?;
        let size = window.inner_size().cast::<f32>();
        let viewport = Rect {
            x: 0.0,
            y: 0.0,
            width: size.width,
            height: (size.height - self.config.appearance.status_bar_height * self.scale_factor)
                .max(1.0),
        };
        let cell = gpu.renderer.cell_size();

        let ws_rects = self.window_layout.compute_workspace_rects(viewport);
        let &(first_ws_id, ws_rect) =
            ws_rects.first().map_or(&(self.window_layout.focused_workspace_id(), viewport), |p| p);

        let tab = self.window_layout.active_tab()?;
        let pane_rects = tab.pane_layout.compute_rects(ws_rect);
        let &(pane_id, pane_rect, pane_edges) = pane_rects.first()?;
        let tab_bar_h = self.tab_bar_height_for(first_ws_id, ws_rect);
        let eff_tbh = if pane_edges.top() { tab_bar_h } else { 0.0 };
        let grid = pane::compute_pane_grid(&pane::PaneGridRequest {
            rect: pane_rect,
            cell_size: (cell.width, cell.height),
            tab_bar_height: eff_tbh,
            prompt_bar_height: 0.0,
            padding: &pane::effective_padding(
                &self.config.appearance.content_padding,
                pane_edges,
                self.scale_factor,
            ),
        });
        Some((pane_id, pane_rect, grid))
    }

    /// Handle server confirming session creation.
    ///
    /// Pops the oldest pending (temporary) session ID, rebinds the pane and
    /// tab state to the real server-assigned session ID, and subscribes for
    /// PTY output.
    fn handle_session_created(&mut self, session_id: SessionId, shell_name: &str) {
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
                shell_name.clone_into(&mut pane.shell_name);
            }
        }

        // Update the workspace tab state so it references the real session.
        self.window_layout.update_tab_session(old_session_id, session_id);

        if let Some(tx) = &self.cmd_tx {
            send_command(tx, ClientCommand::Subscribe { session_ids: vec![session_id] });
        }

        self.report_workspace_tree();
    }

    /// Handle session exit.
    fn handle_session_exited(&mut self, session_id: SessionId, event_loop: &ActiveEventLoop) {
        tracing::info!(session = %session_id, "session exited");
        self.ai_tracker.remove(session_id);
        self.notification_tracker.remove(session_id);
        self.close_pending_notification(session_id);

        let Some(pane_id) = self.session_to_pane.remove(&session_id) else { return };

        // Find which workspace owns this pane via the pane's stored workspace_id.
        // workspace_for_session() only searches tab root sessions, so split panes
        // (which are not registered as tabs) would return None.
        let ws_id = self.panes.get(&pane_id).map(|p| p.workspace_id);

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
            if self.panes.is_empty() {
                self.handle_close_window(event_loop);
                return;
            }
            self.request_redraw();
            return;
        }

        self.panes.remove(&pane_id);
        self.url_caches.remove(&pane_id);

        // Close the pane in the tab that owns it, not necessarily the active tab.
        self.close_exited_pane_in_tab(ws_id, pane_id);

        self.resize_after_layout_change();
        self.report_workspace_tree();
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
        self.update_ai_launch_binding(session_id, &ai_state);

        // Detect conversation reset: if conversation_id changed, clear prompts.
        if let Some(new_conv_id) = ai_state.conversation_id.as_deref() {
            self.maybe_reset_prompts_on_conversation_change(session_id, new_conv_id);
        }

        let provider_enabled = self.config.terminal.ai_provider_enabled(ai_state.provider);
        tracing::debug!(
            session = %session_id,
            provider = ?ai_state.provider,
            state = ?ai_state.state,
            "AI state changed received"
        );
        let notification = self.notification_tracker.on_ai_state_changed(session_id, &ai_state);
        self.ai_tracker.update(session_id, ai_state);
        if let Some(ref payload) = notification {
            self.maybe_fire_notification(payload);
        }
        self.reconcile_split_scroll_for_session(session_id);

        if !provider_enabled {
            return;
        }

        if self.ai_tracker.needs_animation(&self.config.terminal) && !self.animation.running {
            self.start_animation_timer();
        }

        self.request_redraw();
    }

    /// Fire a desktop notification if focus conditions warrant it.
    fn maybe_fire_notification(&mut self, payload: &notifications::NotificationPayload) {
        use scribe_common::config::NotifyCondition;

        let session_is_focused = self.focused_session_id() == Some(payload.session_id);
        let suppress = match self.notification_tracker.config().condition {
            NotifyCondition::WhenUnfocused => self.focus.window_focused,
            NotifyCondition::WhenUnfocusedOrBackgroundTab => {
                self.focus.window_focused && session_is_focused
            }
            NotifyCondition::Always => false,
        };
        if suppress {
            return;
        }

        let summary = self.notification_summary(payload.session_id, &payload.state);
        let body = self
            .session_to_pane
            .get(&payload.session_id)
            .and_then(|pid| self.panes.get(pid))
            .and_then(|pane| pane.latest_prompt.as_deref().or(pane.first_prompt.as_deref()))
            .unwrap_or("")
            .to_owned();

        let session_id = payload.session_id;

        // Record for focus-on-activate fallback (macOS: clicking a
        // notification activates the app but notify-rust cannot capture
        // the click, so the Focused(true) handler checks this).
        self.notification_tracker.set_last_notified(session_id);

        if let Some(tx) = &self.notification_tx {
            let cfg = self.notification_tracker.config();
            let req = notification_dispatcher::NotifReq::Show(notification_dispatcher::ShowReq {
                session_id,
                summary,
                body,
                timeout_mode: cfg.timeout_mode,
                timeout_secs: cfg.timeout_secs,
            });
            if tx.send(req).is_err() {
                tracing::debug!("notification dispatcher closed");
            }
        }

        // Linux/X11 surfaces `request_user_attention` as the WM urgency hint,
        // which GNOME turns into a second "is ready" shell notification.
        // Keep the explicit OS notification there and reserve user-attention
        // requests for platforms where it manifests as dock/taskbar flair.
        // If an update is already announced in the window title, suppress
        // extra attention requests so macOS does not re-surface the update
        // notification text for unrelated AI state changes.
        #[cfg(not(target_os = "linux"))]
        if self.update_available.is_none()
            && let Some(window) = &self.window
        {
            window.request_user_attention(Some(winit::window::UserAttentionType::Informational));
        }
    }

    /// Ask the dispatcher to dismiss the active notification for a
    /// session. Best-effort: macOS treats this as a no-op because
    /// `NSUserNotification` exposes no programmatic dismiss path
    /// through `notify-rust`.
    fn close_pending_notification(&self, session_id: SessionId) {
        if let Some(tx) = &self.notification_tx {
            drop(tx.send(notification_dispatcher::NotifReq::Close { session_id }));
        }
    }

    /// Tell the dispatcher to close every live toast and exit. Called
    /// from the terminal exit paths so a fresh client process does
    /// not inherit lingering notifications it cannot manage.
    fn shutdown_notification_dispatcher(&mut self) {
        if let Some(tx) = self.notification_tx.take() {
            drop(tx.send(notification_dispatcher::NotifReq::Shutdown));
        }
    }

    /// Build a notification summary like "scribe — Ready" or "Scribe — Permission required".
    fn notification_summary(
        &self,
        session_id: SessionId,
        state: &scribe_common::ai_state::AiState,
    ) -> String {
        use scribe_common::ai_state::AiState;

        let state_label = match state {
            AiState::IdlePrompt => "Ready",
            AiState::WaitingForInput => "Waiting for input",
            AiState::PermissionPrompt => "Permission required",
            _ => "Attention",
        };

        let workspace_label = self
            .session_to_pane
            .get(&session_id)
            .and_then(|pid| self.panes.get(pid))
            .and_then(|pane| {
                pane.workspace_name.clone().or_else(|| {
                    self.window_layout
                        .find_workspace(pane.workspace_id)
                        .and_then(|ws| ws.project_root.as_ref())
                        .and_then(|p| p.file_name())
                        .and_then(|n| n.to_str())
                        .map(String::from)
                })
            })
            .unwrap_or_else(|| String::from("Scribe"));

        format!("{workspace_label} — {state_label}")
    }

    /// Clear stale split-scroll state when AI/provider eligibility changes.
    fn reconcile_split_scroll_for_session(&mut self, session_id: SessionId) {
        let Some(pane_id) = self.session_to_pane.get(&session_id).copied() else { return };
        let Some(pane) = self.panes.get_mut(&pane_id) else { return };
        let split_scroll_eligibility = SplitScrollEligibility::for_session(
            session_id,
            &self.ai_tracker,
            &self.config.terminal,
        );
        reconcile_split_scroll(pane, split_scroll_eligibility);
    }

    fn update_ai_launch_binding(
        &mut self,
        session_id: SessionId,
        ai_state: &scribe_common::ai_state::AiProcessState,
    ) {
        let Some(pane_id) = self.session_to_pane.get(&session_id).copied() else { return };
        let Some(pane) = self.panes.get_mut(&pane_id) else { return };

        // Preserve existing conversation_id when the incoming state omits it
        // (e.g. Notification hooks that don't receive session_id from Claude).
        let effective_conversation_id = ai_state.conversation_id.clone().or_else(|| {
            if let restore_state::LaunchKind::Ai { conversation_id, .. } = &pane.launch_binding.kind
            {
                conversation_id.clone()
            } else {
                None
            }
        });

        if let restore_state::LaunchKind::Ai { provider, conversation_id, .. } =
            &pane.launch_binding.kind
        {
            if *provider == ai_state.provider && *conversation_id == effective_conversation_id {
                return;
            }
        }

        pane.launch_binding.kind = restore_state::LaunchKind::Ai {
            provider: ai_state.provider,
            resume_mode: restore_state::AiResumeMode::Resume,
            conversation_id: effective_conversation_id,
        };
        self.mark_restore_dirty();
    }

    /// Handle animation timer tick.
    fn handle_animation_tick(&mut self) {
        let now = Instant::now();
        let dt = now.duration_since(self.last_tick).as_secs_f32();
        self.last_tick = now;
        self.ai_tracker.tick(dt);
        let (sync_pending, sync_flushed) = self.flush_expired_sync_updates(now);

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

        // Edge-scroll: keep scrolling while the cursor is near the pane edge
        // during a selection drag, even when the mouse is not moving.
        let edge_scrolling = self.maybe_edge_scroll();

        let ai_animating = self.ai_tracker.needs_animation(&self.config.terminal);
        let drag_active = self.tab_drag.as_ref().is_some_and(|d| d.dragging);
        if !ai_animating
            && !sync_pending
            && !scrollbar_animating
            && !tab_animating
            && !drag_active
            && !edge_scrolling
        {
            self.animation.running = false;
            self.animation.generation.fetch_add(1, Ordering::Relaxed);
        }

        if ai_animating
            || sync_flushed
            || scrollbar_animating
            || tab_animating
            || drag_active
            || edge_scrolling
        {
            self.request_redraw();
        }
    }

    /// Handle CWD change for a session — store on the pane.
    fn handle_cwd_changed(&mut self, session_id: SessionId, cwd: std::path::PathBuf) {
        let Some(pane_id) = self.session_to_pane.get(&session_id).copied() else { return };
        let Some(pane) = self.panes.get_mut(&pane_id) else { return };
        tracing::debug!(%session_id, ?cwd, "CWD changed");
        pane.cwd = Some(cwd);
    }

    fn handle_session_context_changed(
        &mut self,
        session_id: SessionId,
        context: scribe_common::protocol::SessionContext,
    ) {
        let Some(pane_id) = self.session_to_pane.get(&session_id).copied() else { return };
        let Some(pane) = self.panes.get_mut(&pane_id) else { return };
        tracing::debug!(%session_id, ?context, "session context changed");
        pane.session_context = Some(context);
        self.request_redraw();
    }

    /// Handle title change for a session — update pane title.
    fn handle_title_changed(&mut self, session_id: SessionId, title: &str) {
        if title.trim().is_empty() {
            return;
        }
        let Some(pane_id) = self.session_to_pane.get(&session_id).copied() else { return };
        let Some(pane) = self.panes.get_mut(&pane_id) else { return };
        tracing::debug!(%session_id, %title, "title changed");
        title.clone_into(&mut pane.title);
    }

    /// Handle Codex task-label changes for a session.
    fn handle_codex_task_label_changed(&mut self, session_id: SessionId, task_label: &str) {
        if task_label.trim().is_empty() {
            return;
        }
        let Some(pane_id) = self.session_to_pane.get(&session_id).copied() else { return };
        let Some(pane) = self.panes.get_mut(&pane_id) else { return };
        tracing::debug!(%session_id, %task_label, "codex task label changed");
        pane.codex_task_label = Some(task_label.to_owned());
        self.request_redraw();
    }

    /// Compose the soonest of the cursor-blink and prompt-timer wake
    /// deadlines into a single `Instant`. Returns `None` when neither is
    /// active. Branches that should fire *immediately* call
    /// [`Self::request_redraw`] directly and contribute no deadline.
    fn next_idle_wake_deadline(&mut self) -> Option<Instant> {
        let blink = self.next_blink_wake();
        let timer = self.next_prompt_timer_wake();
        match (blink, timer) {
            (Some(b), Some(t)) => Some(b.min(t)),
            (Some(b), None) => Some(b),
            (None, Some(t)) => Some(t),
            (None, None) => None,
        }
    }

    /// Cursor-blink contribution to the idle wake deadline.
    fn next_blink_wake(&mut self) -> Option<Instant> {
        if !self.cursor.blink_enabled {
            return None;
        }
        let elapsed = self.blink_timer.elapsed();
        if elapsed >= BLINK_INTERVAL {
            // Interval already elapsed — toggle on the next redraw.
            self.request_redraw();
            return None;
        }
        Some(Instant::now() + BLINK_INTERVAL.saturating_sub(elapsed))
    }

    /// The soonest moment any visible prompt bar's elapsed-time counter
    /// needs to redraw. Within the first hour of a prompt the timer ticks
    /// every second (because seconds are visible); past one hour it ticks
    /// once a minute. Returns `None` when no pane has a visible counter.
    /// If a tick is already due, calls [`Self::request_redraw`] and
    /// returns `None`.
    fn next_prompt_timer_wake(&mut self) -> Option<Instant> {
        if !self.config.terminal.prompt_bar.enabled {
            return None;
        }
        let now_system = SystemTime::now();
        let now_instant = Instant::now();
        let mut next: Option<Instant> = None;
        let mut fire_now = false;
        for pane in self.panes.values() {
            if pane.prompt_count == 0 || pane.prompt_ui.dismissed {
                continue;
            }
            let Some(since) = pane.latest_prompt_at else { continue };
            let elapsed = now_system.duration_since(since).unwrap_or(Duration::ZERO);
            let elapsed_secs = elapsed.as_secs();
            let elapsed_subsec = elapsed.saturating_sub(Duration::from_secs(elapsed_secs));
            let wait = if elapsed_secs < 3600 {
                Duration::from_secs(1).saturating_sub(elapsed_subsec)
            } else {
                let remaining_full_secs = 60 - (elapsed_secs % 60);
                Duration::from_secs(remaining_full_secs).saturating_sub(elapsed_subsec)
            };
            if wait.is_zero() {
                fire_now = true;
                continue;
            }
            // Floor at a small minimum so scheduling jitter near a boundary
            // never leaves us in a tight wake-up loop.
            let wait = wait.max(Duration::from_millis(50));
            let deadline = now_instant + wait;
            next = Some(next.map_or(deadline, |prev| prev.min(deadline)));
        }
        if fire_now {
            self.request_redraw();
        }
        next
    }

    /// Record a prompt received from the user and resize the pane if the
    /// prompt bar height changes.
    fn handle_prompt_received(&mut self, session_id: SessionId, text: String) {
        let Some(pane_id) = self.session_to_pane.get(&session_id).copied() else { return };
        let Some(pane) = self.panes.get_mut(&pane_id) else { return };

        let old_line_count = prompt_bar_line_count(pane.prompt_count);

        pane.prompt_count += 1;
        if pane.prompt_count == 1 {
            pane.first_prompt = Some(text);
        } else {
            pane.latest_prompt = Some(text);
        }
        pane.latest_prompt_at = Some(SystemTime::now());

        let new_line_count = prompt_bar_line_count(pane.prompt_count);

        tracing::debug!(
            %session_id,
            prompt_count = pane.prompt_count,
            "prompt received"
        );

        if self.config.terminal.prompt_bar.enabled && new_line_count != old_line_count {
            self.resize_after_prompt_bar_height_change();
        }
        self.request_redraw();
    }

    /// Clear prompt state for the pane associated with a session.
    fn clear_pane_prompts(&mut self, session_id: SessionId) {
        let Some(pane_id) = self.session_to_pane.get(&session_id).copied() else { return };
        let Some(pane) = self.panes.get_mut(&pane_id) else { return };

        let had_prompts = pane.prompt_count > 0;
        let was_dismissed = pane.prompt_ui.dismissed;
        pane.first_prompt = None;
        pane.latest_prompt = None;
        pane.latest_prompt_at = None;
        pane.prompt_count = 0;
        pane.last_conversation_id = None;
        pane.prompt_ui.dismissed = false;

        if self.config.terminal.prompt_bar.enabled && (had_prompts || was_dismissed) {
            self.resize_after_prompt_bar_height_change();
        }
    }

    /// Reset prompt state when the conversation ID changes for a session.
    ///
    /// Called from `handle_ai_state_changed` when a new `conversation_id` is
    /// seen. If the ID differs from the last recorded one, all prompt fields
    /// are cleared and the pane is resized if the prompt bar was visible.
    fn maybe_reset_prompts_on_conversation_change(
        &mut self,
        session_id: SessionId,
        new_conv_id: &str,
    ) {
        let Some(pane_id) = self.session_to_pane.get(&session_id).copied() else { return };
        let Some(pane) = self.panes.get_mut(&pane_id) else { return };
        let changed = pane.last_conversation_id.as_deref().is_some_and(|old| old != new_conv_id);
        pane.last_conversation_id = Some(new_conv_id.to_owned());
        if changed {
            let old_lines = prompt_bar_line_count(pane.prompt_count);
            let was_dismissed = pane.prompt_ui.dismissed;
            pane.first_prompt = None;
            pane.latest_prompt = None;
            pane.latest_prompt_at = None;
            pane.prompt_count = 0;
            pane.prompt_ui.dismissed = false;
            if self.config.terminal.prompt_bar.enabled && (old_lines > 0 || was_dismissed) {
                self.resize_after_prompt_bar_height_change();
            }
        }
    }

    /// Restore prompt bar state from a saved cold restart snapshot during
    /// hot restart reattach.
    ///
    /// The previous client saved a snapshot (including prompt state) before
    /// exiting, but `SessionList` does not carry prompt fields.  This reads
    /// the snapshot's `LaunchRecord` entries and copies prompt state to live
    /// panes matched by `conversation_id`.
    fn apply_snapshot_prompt_state(&mut self) {
        let Some((snapshot, _)) = self.restore_store.claim_first_window() else {
            return;
        };

        // Build conversation_id → prompt state from the snapshot.
        let mut prompt_map: HashMap<String, SnapshotPromptState> = HashMap::new();
        for record in snapshot.launches {
            if record.prompt_count == 0 {
                continue;
            }
            if let restore_state::LaunchKind::Ai { conversation_id: Some(conv_id), .. } =
                record.kind
            {
                prompt_map.insert(
                    conv_id,
                    SnapshotPromptState {
                        first: record.first_prompt,
                        latest: record.latest_prompt,
                        latest_at: record
                            .latest_prompt_at
                            .map(|secs| SystemTime::UNIX_EPOCH + Duration::from_secs(secs)),
                        count: record.prompt_count,
                    },
                );
            }
        }

        if prompt_map.is_empty() {
            return;
        }

        let mut restored_any = false;
        for pane in self.panes.values_mut() {
            if pane.prompt_count > 0 {
                continue;
            }
            let Some(conv_id) = pane.last_conversation_id.as_deref() else { continue };
            let Some(state) = prompt_map.remove(conv_id) else { continue };
            pane.first_prompt = state.first;
            pane.latest_prompt = state.latest;
            pane.latest_prompt_at = state.latest_at;
            pane.prompt_count = state.count;
            restored_any = true;
        }

        if restored_any && self.config.terminal.prompt_bar.enabled {
            self.resize_after_prompt_bar_height_change();
        }
    }

    /// Handle explicit Codex task-label clearing for a session.
    fn handle_codex_task_label_cleared(&mut self, session_id: SessionId) {
        let Some(pane_id) = self.session_to_pane.get(&session_id).copied() else { return };
        let Some(pane) = self.panes.get_mut(&pane_id) else { return };
        tracing::debug!(%session_id, "codex task label cleared");
        pane.codex_task_label = None;
        self.request_redraw();
    }

    /// Handle a prompt-mark event (OSC 133).
    fn handle_prompt_mark(
        &mut self,
        session_id: SessionId,
        kind: PromptMarkKind,
        click_events: bool,
    ) {
        let Some(pane_id) = self.session_to_pane.get(&session_id).copied() else { return };
        let Some(pane) = self.panes.get_mut(&pane_id) else { return };
        match kind {
            PromptMarkKind::PromptStart => {
                let history = pane.term.grid().history_size();
                let screen_lines = pane.term.grid().screen_lines();
                let cursor_line = pane.term.grid().cursor.point.line.0;
                let abs_pos = history + usize::try_from(cursor_line.max(0)).unwrap_or(usize::MAX);
                pane.prompt_marks.push(abs_pos);
                // Prune marks that have been evicted from scrollback.
                // The maximum valid abs_pos is history_size + screen_lines - 1.
                // Any mark beyond that ceiling no longer exists in the grid.
                let max_valid = history + screen_lines;
                pane.prompt_marks.retain(|&m| m <= max_valid);
                pane.prompt_ui.click_events = click_events;
                pane.input_start = None;
            }
            PromptMarkKind::PromptEnd => {
                let history = pane.term.grid().history_size();
                let cursor_line = pane.term.grid().cursor.point.line.0;
                let cursor_col = pane.term.grid().cursor.point.column.0;
                let abs_line = history + usize::try_from(cursor_line.max(0)).unwrap_or(usize::MAX);
                pane.input_start = Some((abs_line, cursor_col));
            }
            PromptMarkKind::CommandStart | PromptMarkKind::CommandEnd => {
                pane.input_start = None;
            }
        }
    }

    /// Handle git branch change for a session — store on the pane.
    fn handle_git_branch(&mut self, session_id: SessionId, branch: Option<String>) {
        let Some(pane_id) = self.session_to_pane.get(&session_id).copied() else { return };
        let Some(pane) = self.panes.get_mut(&pane_id) else { return };
        tracing::debug!(%session_id, ?branch, "git branch updated");
        pane.git_branch = branch;
    }

    /// Handle full workspace info from server — update name, accent color,
    /// and, for legacy reconnect fallback only, patch the parent split
    /// direction once.
    fn handle_workspace_info(&mut self, update: WorkspaceInfoUpdate) {
        tracing::debug!(
            workspace_id = %update.workspace_id,
            name = ?update.name,
            accent_color = %update.accent_color,
            split_direction = ?update.split_direction,
            "workspace info received"
        );
        if let Some(ws) = self.window_layout.find_workspace_mut(update.workspace_id) {
            ws.name = update.name;
            ws.project_root = update.project_root;
            if let Some(color) = parse_hex_color(&update.accent_color) {
                ws.accent_color = color;
            }
        }

        // Old servers reconnect without a workspace tree; in that case the
        // fallback layout is a linear chain that needs a one-time direction
        // patch per workspace. Routine WorkspaceInfo updates after startup
        // must not rewrite the live split topology.
        if self.legacy_workspace_direction_updates.remove(&update.workspace_id) {
            if let Some(dir) = update.split_direction {
                self.window_layout
                    .update_split_direction_for(update.workspace_id, from_layout_direction(dir));
            }
        }
    }

    /// Handle workspace auto-naming — update the workspace slot, pane names,
    /// and project root. An empty name signals the workspace is no longer
    /// inside a configured root, clearing the previous name.
    fn handle_workspace_named(
        &mut self,
        workspace_id: WorkspaceId,
        name: &str,
        project_root: Option<std::path::PathBuf>,
    ) {
        tracing::debug!(%workspace_id, %name, "workspace named");

        let name_opt = if name.is_empty() { None } else { Some(name.to_owned()) };

        // Update the workspace slot name and project root.
        if let Some(ws) = self.window_layout.find_workspace_mut(workspace_id) {
            ws.name.clone_from(&name_opt);
            ws.project_root = project_root;
        }

        for pane in self.panes.values_mut() {
            if pane.workspace_id == workspace_id {
                pane.workspace_name.clone_from(&name_opt);
            }
        }
    }

    fn handle_config_changed(&mut self) {
        let new_config = match scribe_common::config::load_config() {
            Ok(c) => c,
            Err(error) => {
                tracing::warn!("config reload failed: {error}");
                return;
            }
        };
        let plan = ConfigReloadPlan::analyze(&self.config, &new_config);

        self.reload_theme_if_needed(&new_config, plan.theme_changed());
        self.reload_fonts_if_needed(&new_config, plan.font_changed());
        if plan.hide_codex_logs_changed() {
            self.mark_all_panes_dirty();
        }
        self.apply_cursor_config(&new_config);
        self.apply_opacity_change(&new_config, plan.opacity_changed());
        self.bindings = input::Bindings::parse(&new_config.keybindings);
        self.ai_tracker.reconfigure(new_config.terminal.ai_session.claude_states.clone());
        self.notification_tracker.reconfigure(new_config.notifications.clone());
        self.config = new_config;
        self.finish_config_reload(&plan);
    }

    fn reload_theme_if_needed(&mut self, new_config: &ScribeConfig, theme_changed: bool) {
        if !theme_changed {
            return;
        }

        let new_theme = resolve_theme(new_config);
        if self.theme == new_theme {
            return;
        }

        if let Some(gpu) = &mut self.gpu {
            gpu.renderer.set_theme(&new_theme);
        }
        self.theme = new_theme;
        self.mark_all_panes_dirty();
    }

    fn reload_fonts_if_needed(&mut self, new_config: &ScribeConfig, font_changed: bool) {
        if !font_changed {
            return;
        }

        let Some(gpu) = &mut self.gpu else { return };
        let size =
            (new_config.appearance.font_size + f32::from(self.zoom_level)) * self.scale_factor;
        let params = scribe_renderer::atlas::FontParams {
            family: new_config.appearance.font.clone(),
            size: size.max(6.0),
            weight: new_config.appearance.font_weight,
            weight_bold: new_config.appearance.font_weight_bold,
            ligatures: new_config.appearance.ligatures,
            line_padding: new_config.appearance.line_padding,
        };
        gpu.renderer.rebuild_atlas(&gpu.device, &gpu.queue, &params);
    }

    fn mark_all_panes_dirty(&mut self) {
        for pane in self.panes.values_mut() {
            pane.content_dirty = true;
        }
    }

    fn apply_cursor_config(&mut self, new_config: &ScribeConfig) {
        if let Some(gpu) = &mut self.gpu {
            gpu.renderer.set_cursor_shape(new_config.appearance.cursor_shape);
        }
        self.cursor.blink_enabled = new_config.appearance.cursor_blink;
        if !self.cursor.blink_enabled {
            self.cursor.visible = true;
        }
    }

    fn apply_opacity_change(&mut self, new_config: &ScribeConfig, opacity_changed: bool) {
        if !opacity_changed {
            return;
        }

        if !self.window_transparent && new_config.appearance.opacity < 1.0 {
            tracing::warn!(
                "opacity < 1.0 requires restart to take effect \
                 (window was created without transparency)"
            );
            return;
        }

        self.opacity = new_config.appearance.opacity;
    }

    fn finish_config_reload(&mut self, plan: &ConfigReloadPlan) {
        if self.ai_tracker.needs_animation(&self.config.terminal) && !self.animation.running {
            self.start_animation_timer();
        }
        if let Some(tx) = &self.cmd_tx {
            send_command(tx, ClientCommand::ConfigReloaded);
        }
        if plan.needs_layout_resize() {
            self.resize_all_workspace_panes();
        }
        if plan.prompt_bar_changed() {
            self.resize_after_prompt_bar_height_change();
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
        use alacritty_terminal::grid::Dimensions as _;

        if self.panes.is_empty() {
            return;
        }
        let Some(gpu) = &self.gpu else { return };
        let cell = gpu.renderer.cell_size();
        let ws_viewport = workspace_viewport(
            &gpu.surface_config,
            self.config.appearance.status_bar_height * self.scale_factor,
        );

        let ws_rects = self.window_layout.compute_workspace_rects(ws_viewport);
        let expected_rects = collect_expected_pane_rects(&self.window_layout, &ws_rects);

        // Build a ws_id → tab_bar_height map for staleness checking.
        let ws_heights: std::collections::HashMap<WorkspaceId, f32> = ws_rects
            .iter()
            .map(|(ws_id, ws_rect)| (*ws_id, self.tab_bar_height_for(*ws_id, *ws_rect)))
            .collect();

        let mut any_stale = false;
        for (pid, rect, edges) in &expected_rects {
            let Some(pane) = self.panes.get(pid) else { continue };
            let tab_bar_h = ws_heights
                .get(&pane.workspace_id)
                .copied()
                .unwrap_or_else(|| self.effective_tab_bar_height());
            let tbh = if edges.top() { tab_bar_h } else { 0.0 };
            let pb_font_scale =
                self.config.terminal.prompt_bar.font_size / self.config.appearance.font_size;
            let pb_cell_h = cell.height * pb_font_scale;
            let pbh = pane.prompt_bar_height(pb_cell_h, self.config.terminal.prompt_bar.enabled);
            let expected = pane::compute_pane_grid(&pane::PaneGridRequest {
                rect: *rect,
                cell_size: (cell.width, cell.height),
                tab_bar_height: tbh,
                prompt_bar_height: pbh,
                padding: &pane::effective_padding(
                    &self.config.appearance.content_padding,
                    *edges,
                    self.scale_factor,
                ),
            });
            if pane.grid.cols != expected.cols || pane.grid.rows != expected.rows {
                any_stale = true;
                break;
            }
        }

        if any_stale {
            tracing::info!("pane grids out of sync with layout — forcing pane resize");
            self.resize_all_workspace_panes();
        }

        // Sync pane.term dimensions to pane.grid.  handle_screen_snapshot may
        // leave the term at snapshot dimensions briefly; this ensures the term
        // matches the grid before the first render.
        for pane in self.panes.values_mut() {
            let term_ok = pane.term.columns() == usize::from(pane.grid.cols)
                && pane.term.screen_lines() == usize::from(pane.grid.rows);
            if !term_ok {
                pane.resize_term_only(pane.grid.cols, pane.grid.rows);
            }
        }
    }

    /// Render one frame: splash while waiting for PTY output, terminal after.
    fn handle_redraw(&mut self) {
        self.sync_surface_to_window();
        let request_redraw = self.drain_pending_output_frames();
        let Some((frame, view)) = self.acquire_surface_frame() else { return };

        self.dismiss_splash_if_ready();
        if self.splash.active {
            self.render_splash_frame(frame, &view);
            return;
        }

        self.render_terminal_frame(frame, &view, request_redraw);
    }

    fn acquire_surface_frame(&mut self) -> Option<(wgpu::SurfaceTexture, wgpu::TextureView)> {
        let gpu = self.gpu.as_mut()?;
        let frame = match gpu.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(texture)
            | wgpu::CurrentSurfaceTexture::Suboptimal(texture) => texture,
            other => {
                tracing::warn!(?other, "failed to acquire surface texture");
                return None;
            }
        };
        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());
        Some((frame, view))
    }

    fn dismiss_splash_if_ready(&mut self) {
        if !self.splash.active || !self.splash.content_ready {
            return;
        }

        let elapsed_ok =
            self.splash_first_rendered.is_some_and(|time| time.elapsed() >= MIN_SPLASH_DURATION);
        if elapsed_ok {
            self.splash.active = false;
            if let Some(gpu) = &mut self.gpu {
                gpu.splash = None;
            }
        }
    }

    fn render_splash_frame(&mut self, frame: wgpu::SurfaceTexture, view: &wgpu::TextureView) {
        let Some(gpu) = &mut self.gpu else { return };
        if let Some(splash) = &gpu.splash {
            let mut encoder = gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("splash encoder"),
            });
            splash.render(&mut encoder, view);
            gpu.queue.submit(std::iter::once(encoder.finish()));
            self.notify_pre_present();
            frame.present();
        }

        if self.splash_first_rendered.is_none() {
            self.splash_first_rendered = Some(Instant::now());
        }
        self.send_deferred_list_sessions();
        if self.splash.content_ready {
            self.request_redraw();
        }
    }

    fn render_terminal_frame(
        &mut self,
        frame: wgpu::SurfaceTexture,
        view: &wgpu::TextureView,
        request_redraw: bool,
    ) {
        let (full_viewport, ws_viewport, cell_size) = {
            let Some(gpu) = self.gpu.as_ref() else { return };
            (
                viewport_rect(&gpu.surface_config),
                workspace_viewport(
                    &gpu.surface_config,
                    self.config.appearance.status_bar_height * self.scale_factor,
                ),
                (gpu.renderer.cell_size().width, gpu.renderer.cell_size().height),
            )
        };
        let prepared = self.prepare_terminal_frame(full_viewport, ws_viewport, cell_size);
        let mut refresh_window_title = false;
        let Some((mut all_instances, tab_hits, tab_close_hits, tab_eq_hits, tab_tt_hits)) =
            self.build_terminal_frame_instances(&prepared)
        else {
            return;
        };
        self.tab_hit_targets = tab_hits;
        self.tab_close_hit_targets = tab_close_hits;
        self.tab_bar_equalize_targets = tab_eq_hits;
        self.tab_bar_tooltip_targets = tab_tt_hits;
        self.apply_terminal_frame_overlays(
            &prepared,
            &mut all_instances,
            &mut refresh_window_title,
        );
        self.present_terminal_frame(
            frame,
            view,
            &all_instances,
            TerminalFramePresentation { refresh_window_title, request_redraw },
        );
    }

    fn prepare_terminal_frame(
        &mut self,
        full_viewport: Rect,
        ws_viewport: Rect,
        cell_size: (f32, f32),
    ) -> PreparedFrame {
        let ws_rects = self.window_layout.compute_workspace_rects(ws_viewport);
        let focused_ws_id = self.window_layout.focused_workspace_id();
        let multi_workspace = self.window_layout.workspace_count() > 1;
        let ansi_colors = linearise_ansi_colors(&self.theme.ansi_colors);
        let workspace_scene = self.collect_workspace_frame_scene(
            &ws_rects,
            WorkspaceSceneContext {
                focused_ws_id,
                multi_workspace,
                cell_size,
                ansi_colors: &ansi_colors,
            },
        );
        let border_colors =
            self.collect_workspace_border_colors(&workspace_scene.pane_rects, &ansi_colors);
        self.update_prepared_frame_state(&workspace_scene.pane_rects);
        let frame_style = self.prepare_frame_style(cell_size, &ansi_colors);
        let ws_tab_bar_heights = workspace_scene
            .ws_tab_bar_data
            .iter()
            .map(|data| (data.ws_id, data.tab_bar_height))
            .collect();
        PreparedFrame {
            full_viewport,
            cell_size,
            pane_rects: workspace_scene.pane_rects,
            dividers: workspace_scene.dividers,
            ws_dividers: self.window_layout.collect_workspace_dividers(ws_viewport),
            ws_tab_bar_data: workspace_scene.ws_tab_bar_data,
            focused_pane: workspace_scene.focused_pane,
            border_colors,
            focused_ws_rect: focused_workspace_rect(&ws_rects, focused_ws_id, multi_workspace),
            ws_tab_bar_heights,
            prompt_bar_cell_size: frame_style.prompt_bar_cell_size,
            prompt_bar_at_top: frame_style.prompt_bar_at_top,
            cursor_visible: self.cursor.visible && self.focus.window_focused,
            tab_colors: frame_style.tab_colors,
            sb_colors: frame_style.sb_colors,
            divider_color: frame_style.divider_color,
            accent_color: frame_style.accent_color,
            focus_border_color: frame_style.focus_border_color,
            focus_border_width: frame_style.focus_border_width,
            scrollbar_width: frame_style.scrollbar_width,
            scrollbar_color: frame_style.scrollbar_color,
            indicator_height: frame_style.indicator_height,
            prompt_bar_colors: frame_style.prompt_bar_colors,
            status: self.frame_status_snapshot(),
        }
    }

    fn collect_workspace_frame_scene(
        &self,
        ws_rects: &WorkspaceRects,
        ctx: WorkspaceSceneContext<'_>,
    ) -> WorkspaceFrameScene {
        let mut pane_rects = Vec::new();
        let mut dividers = Vec::new();
        let mut focused_pane = PaneId::from_raw(u32::MAX);
        let mut ws_tab_bar_data = Vec::new();

        for (ws_id, ws_rect) in ws_rects {
            let Some(ws) = self.window_layout.find_workspace(*ws_id) else { continue };
            let Some(tab) = ws.active_tab() else { continue };

            let tab_bar_data = self.build_workspace_tab_bar_data(
                WorkspaceSceneEntry { ws_id: *ws_id, ws_rect: *ws_rect, ws, tab },
                ctx,
            );
            let rects_with_edges = tab.pane_layout.compute_rects(*ws_rect);
            pane_rects.extend(rects_with_edges.iter().map(|&(id, rect, _)| (id, rect)));

            let mut ws_dividers = divider::collect_dividers(tab.pane_layout.root(), *ws_rect);
            let scaled_pad = pane::effective_padding(
                &self.config.appearance.content_padding,
                PaneEdges::all_external(),
                self.scale_factor,
            );
            divider::apply_viewport_insets(
                &mut ws_dividers,
                *ws_rect,
                &scaled_pad,
                tab_bar_data.tab_bar_height,
            );
            dividers.extend(ws_dividers);

            if *ws_id == ctx.focused_ws_id {
                focused_pane = tab.focused_pane;
            }
            ws_tab_bar_data.push(tab_bar_data);
        }

        WorkspaceFrameScene { pane_rects, dividers, ws_tab_bar_data, focused_pane }
    }

    fn build_workspace_tab_bar_data(
        &self,
        entry: WorkspaceSceneEntry<'_>,
        ctx: WorkspaceSceneContext<'_>,
    ) -> tab_bar::WorkspaceTabBarData {
        let tabs = entry
            .ws
            .tabs
            .iter()
            .enumerate()
            .map(|(index, tab_state)| {
                let pane_count = tab_state.pane_layout.all_pane_ids().len();
                let title = tab_title(
                    pane_count,
                    index,
                    tab_state.session_id,
                    &self.session_to_pane,
                    &self.panes,
                );
                let ai_indicator = self.ai_tracker.tab_indicator_color(
                    tab_state.session_id,
                    ctx.ansi_colors,
                    &self.config.terminal,
                );
                tab_bar::TabData { title, is_active: index == entry.ws.active_tab, ai_indicator }
            })
            .collect();
        let badge_cols = tab_bar::badge_columns(entry.ws.name.as_deref(), ctx.multi_workspace);
        let pane_count = entry.tab.pane_layout.all_pane_ids().len();
        let has_multiple_panes = pane_count > 1;
        let trailing_controls =
            tab_bar::TabBarTrailingControls { gear: false, equalize: has_multiple_panes };
        let row_height = self.config.appearance.tab_height * self.scale_factor;
        let tab_bar_height = tab_bar::compute_tab_bar_height(tab_bar::TabBarHeightRequest {
            tab_count: entry.ws.tabs.len(),
            ws_width: entry.ws_rect.width,
            tab_width_chars: self.config.appearance.tab_width,
            cell_w: ctx.cell_size.0,
            row_height,
            badge_cols,
            trailing_controls,
        });
        let total_cols = workspace_columns(entry.ws_rect, ctx.cell_size.0);
        let trailing_cols = trailing_controls.columns();
        let tab_width = usize::from(self.config.appearance.tab_width).max(1);
        let available_for_tabs =
            total_cols.saturating_sub(badge_cols).saturating_sub(trailing_cols);
        let tabs_per_row = (available_for_tabs / tab_width).max(1);

        tab_bar::WorkspaceTabBarData {
            ws_id: entry.ws_id,
            ws_rect: entry.ws_rect,
            tabs,
            badge: ctx
                .multi_workspace
                .then(|| {
                    make_workspace_badge(
                        entry.ws.name.as_deref(),
                        &self.config.workspaces.badge_colors,
                    )
                })
                .flatten(),
            has_multiple_panes,
            tab_bar_height,
            active_tab_pixel_range: compute_active_tab_pixel_range(ActiveTabPixelRangeRequest {
                ws_rect_x: entry.ws_rect.x,
                active_tab_idx: entry.ws.active_tab,
                tab_width: self.config.appearance.tab_width,
                badge_cols,
                tabs_per_row,
                cell_w: ctx.cell_size.0,
            }),
        }
    }

    fn collect_workspace_border_colors(
        &self,
        pane_rects: &[(PaneId, Rect)],
        ansi_colors: &[[f32; 4]; 16],
    ) -> HashMap<PaneId, [f32; 4]> {
        pane_rects
            .iter()
            .filter_map(|(pane_id, _)| {
                let pane = self.panes.get(pane_id)?;
                let ws = self.window_layout.find_workspace(pane.workspace_id)?;
                let ws_pane_ids: Vec<PaneId> =
                    ws.tabs.iter().flat_map(|tab| tab.pane_layout.all_pane_ids()).collect();
                let session_ids: Vec<SessionId> = ws_pane_ids
                    .iter()
                    .filter_map(|pid| self.panes.get(pid).map(|pane_state| pane_state.session_id))
                    .collect();
                let color = self.ai_tracker.workspace_border_color(
                    &session_ids,
                    ansi_colors,
                    &self.config.terminal,
                )?;
                Some((*pane_id, color))
            })
            .collect()
    }

    fn update_prepared_frame_state(&mut self, pane_rects: &[(PaneId, Rect)]) {
        if self.cursor.blink_enabled && self.blink_timer.elapsed() >= BLINK_INTERVAL {
            self.cursor.visible = !self.cursor.visible;
            self.blink_timer = Instant::now();
        }
        for (pane_id, rect) in pane_rects {
            if let Some(pane) = self.panes.get_mut(pane_id) {
                pane.rect = *rect;
            }
        }
    }

    fn frame_status_snapshot(&self) -> FrameStatusSnapshot {
        let focused_pane =
            self.window_layout.active_tab().and_then(|tab| self.panes.get(&tab.focused_pane));
        FrameStatusSnapshot {
            focused_pane_cwd: focused_pane.and_then(|pane| pane.cwd.clone()),
            focused_pane_git: focused_pane.and_then(|pane| pane.git_branch.clone()),
            focused_pane_display_context: focused_pane
                .and_then(|pane| pane.session_context.as_ref())
                .map(|context| {
                    let host_label = if context.remote {
                        context.host.clone().unwrap_or_default()
                    } else {
                        String::new()
                    };
                    (host_label, context.tmux_session.clone())
                }),
            focused_ws_name: self.window_layout.focused_workspace().and_then(|ws| ws.name.clone()),
            session_count: self.panes.len(),
        }
    }

    fn prepare_frame_style(
        &self,
        cell_size: (f32, f32),
        ansi_colors: &[[f32; 4]; 16],
    ) -> PreparedFrameStyle {
        let accent_color = scribe_renderer::srgb_to_linear_rgba(self.theme.chrome.accent);
        let focus_border_color =
            self.config.appearance.focus_border_color.as_deref().map_or(accent_color, |hex| {
                scribe_common::theme::hex_to_rgba(hex)
                    .map_or(accent_color, scribe_renderer::srgb_to_linear_rgba)
            });
        PreparedFrameStyle {
            prompt_bar_cell_size: prompt_bar_cell_size(
                cell_size,
                self.config.terminal.prompt_bar.font_size,
                self.config.appearance.font_size,
            ),
            prompt_bar_at_top: self.config.terminal.prompt_bar.position
                == scribe_common::config::PromptBarPosition::Top,
            tab_colors: tab_bar::TabBarColors::from(&self.theme.chrome),
            sb_colors: status_bar::StatusBarColors::from_theme(&self.theme.chrome, ansi_colors),
            divider_color: scribe_renderer::srgb_to_linear_rgba(self.theme.chrome.divider),
            accent_color,
            focus_border_color,
            focus_border_width: self.config.appearance.focus_border_width.clamp(1.0, 10.0)
                * self.scale_factor,
            scrollbar_width: self.config.appearance.scrollbar_width.clamp(2.0, 20.0)
                * self.scale_factor,
            scrollbar_color: self.resolve_scrollbar_color(),
            indicator_height: self.config.terminal.ai_session.indicator_height.clamp(1.0, 10.0)
                * self.scale_factor,
            prompt_bar_colors: self.resolve_prompt_bar_colors(),
        }
    }

    fn build_terminal_frame_instances(
        &mut self,
        prepared: &PreparedFrame,
    ) -> Option<BuiltFrameInstances> {
        let gpu = self.gpu.as_mut()?;
        let frame_layout = FrameLayout {
            pane_rects: &prepared.pane_rects,
            dividers: &prepared.dividers,
            ws_dividers: &prepared.ws_dividers,
            ws_tab_bar_data: &prepared.ws_tab_bar_data,
            cell_size: prepared.cell_size,
            focused_pane: prepared.focused_pane,
            padding: &self.config.appearance.content_padding,
            scale_factor: self.scale_factor,
            focused_ws_rect: prepared.focused_ws_rect,
            prompt_bar: PromptBarLayoutState {
                cell_size: prepared.prompt_bar_cell_size,
                at_top: prepared.prompt_bar_at_top,
                enabled: self.config.terminal.prompt_bar.enabled,
                hover: self.prompt_bar_hover,
                pressed: self.prompt_bar_pressed,
            },
            scroll_pin: ScrollPinLayoutState {
                enabled: self.config.terminal.scroll.scroll_pin,
                hover: self.scroll_pin_hover,
            },
        };
        let frame_style = FrameStyle {
            border_colors: &prepared.border_colors,
            tab_colors: &prepared.tab_colors,
            divider_color: prepared.divider_color,
            accent_color: prepared.accent_color,
            focus_border_color: prepared.focus_border_color,
            focus_border_width: prepared.focus_border_width,
            scrollbar_width: prepared.scrollbar_width,
            scrollbar_color: prepared.scrollbar_color,
            indicator_height: prepared.indicator_height,
            prompt_bar_colors: prepared.prompt_bar_colors,
        };
        let frame_interaction = FrameInteraction {
            cursor_visible: prepared.cursor_visible,
            tab_width: self.config.appearance.tab_width,
            active_selection: self.active_selection.as_ref(),
            search_matches: self.search_overlay.matches(),
            search_current_index: self.search_overlay.current_match_index(),
            hovered_tab_close: self.hovered_tab_close,
            hovered_tab: self.hovered_tab,
            tab_drag: self.tab_drag.as_ref(),
            tab_drag_offsets: &self.tab_drag_offsets,
        };
        Some(build_all_instances(InstanceBuildContext {
            backend: RenderBackend {
                renderer: &mut gpu.renderer,
                device: &gpu.device,
                queue: &gpu.queue,
            },
            panes: &mut self.panes,
            layout: &frame_layout,
            style: &frame_style,
            interaction: &frame_interaction,
        }))
    }

    fn apply_terminal_frame_overlays(
        &mut self,
        prepared: &PreparedFrame,
        all_instances: &mut Vec<CellInstance>,
        refresh_window_title: &mut bool,
    ) {
        self.apply_url_underline_overlay(prepared, all_instances);
        *refresh_window_title |= self.apply_status_bar_overlay(prepared, all_instances);
        self.apply_modal_overlays(prepared, all_instances);
        self.apply_prompt_tooltip_overlay(prepared, all_instances);
        self.apply_active_tooltip_overlay(prepared, all_instances);
        self.apply_palette_or_search_overlay(prepared, all_instances);
        if self.opacity < 1.0 {
            apply_opacity_to_instances(all_instances, self.opacity);
        }
    }

    fn apply_url_underline_overlay(
        &mut self,
        prepared: &PreparedFrame,
        all_instances: &mut Vec<CellInstance>,
    ) {
        let fallback_tbh = prepared.ws_tab_bar_data.first().map_or(0.0, |data| data.tab_bar_height);
        apply_url_underlines(
            all_instances,
            &mut self.url_caches,
            &self.panes,
            UrlUnderlineOverlayContext {
                pane_rects: &prepared.pane_rects,
                ws_tab_bar_heights: &prepared.ws_tab_bar_heights,
                fallback_tbh,
                cell_size: prepared.cell_size,
                focused_pane_id: self.window_layout.active_tab().map(|tab| tab.focused_pane),
                hovered_url: self.hovered_url.as_ref(),
                padding: &self.config.appearance.content_padding,
                scale_factor: self.scale_factor,
                ctrl_held: self.modifiers.control_key(),
                prompt_bar: PromptBarOverlayState {
                    enabled: self.config.terminal.prompt_bar.enabled,
                    cell_h: prepared.prompt_bar_cell_size.1,
                    position: if prepared.prompt_bar_at_top {
                        PromptBarOverlayPosition::Top
                    } else {
                        PromptBarOverlayPosition::Bottom
                    },
                },
            },
        );
    }

    fn apply_status_bar_overlay(
        &mut self,
        prepared: &PreparedFrame,
        all_instances: &mut Vec<CellInstance>,
    ) -> bool {
        let Some(gpu) = self.gpu.as_mut() else { return false };
        let time_str = current_time_str();
        self.sys_stats.maybe_refresh();
        let tmux_label = prepared
            .status
            .focused_pane_display_context
            .as_ref()
            .and_then(|(_, label)| label.as_deref());
        let host_label = prepared
            .status
            .focused_pane_display_context
            .as_ref()
            .and_then(|(host, _)| (!host.is_empty()).then_some(host.as_str()))
            .unwrap_or(self.hostname.as_str());
        let sb_data = status_bar::StatusBarData {
            connected: self.connection.server_connected,
            show_equalize: prepared.focused_ws_rect.is_some(),
            workspace_name: prepared.status.focused_ws_name.as_deref(),
            cwd: prepared.status.focused_pane_cwd.as_deref(),
            git_branch: prepared.status.focused_pane_git.as_deref(),
            session_count: prepared.status.session_count,
            host_label,
            tmux_label,
            time: &time_str,
            update_available: self.update_available.as_deref(),
            update_progress: self.update_progress.as_ref(),
            sys_stats: Some(self.sys_stats.stats()),
            stats_config: Some(&self.config.terminal.status_bar_stats),
        };
        let mut status_bar_resolve_glyph =
            |ch: char| gpu.renderer.resolve_glyph(&gpu.device, &gpu.queue, ch);
        let sb_hits = status_bar::build_status_bar(status_bar::StatusBarBuildContext {
            out: all_instances,
            window_rect: prepared.full_viewport,
            cell_size: prepared.cell_size,
            status_bar_height: self.config.appearance.status_bar_height * self.scale_factor,
            colors: &prepared.sb_colors,
            data: &sb_data,
            resolve_glyph: &mut status_bar_resolve_glyph,
        });
        let prev_update_clickable = self.status_bar_update_rect.is_some();
        self.status_bar_gear_rect = sb_hits.gear_rect;
        self.status_bar_equalize_rect = sb_hits.equalize_rect;
        self.status_bar_update_rect = sb_hits.update_rect;
        self.status_bar_tooltip_targets = sb_hits.tooltip_targets;
        prev_update_clickable != self.status_bar_update_rect.is_some()
    }

    fn apply_modal_overlays(
        &mut self,
        prepared: &PreparedFrame,
        all_instances: &mut Vec<CellInstance>,
    ) {
        let Some(gpu) = self.gpu.as_mut() else { return };
        if let Some(dialog) = &mut self.close_dialog {
            let mut close_dialog_resolve_glyph =
                |ch: char| gpu.renderer.resolve_glyph(&gpu.device, &gpu.queue, ch);
            dialog.build_instances(close_dialog::CloseDialogBuildContext {
                out: all_instances,
                viewport: prepared.full_viewport,
                cell_size: prepared.cell_size,
                chrome: &self.theme.chrome,
                resolve_glyph: &mut close_dialog_resolve_glyph,
            });
        }
        if let Some(dialog) = &mut self.update_dialog {
            let mut update_dialog_resolve_glyph =
                |ch: char| gpu.renderer.resolve_glyph(&gpu.device, &gpu.queue, ch);
            dialog.build_instances(update_dialog::UpdateDialogBuildContext {
                out: all_instances,
                viewport: prepared.full_viewport,
                cell_size: prepared.cell_size,
                chrome: &self.theme.chrome,
                resolve_glyph: &mut update_dialog_resolve_glyph,
            });
        }
        if let Some(menu) = &mut self.context_menu {
            let mut context_menu_resolve_glyph =
                |ch: char| gpu.renderer.resolve_glyph(&gpu.device, &gpu.queue, ch);
            menu.build_instances(context_menu::ContextMenuBuildContext {
                out: all_instances,
                viewport: prepared.full_viewport,
                cell_size: prepared.cell_size,
                chrome: &self.theme.chrome,
                resolve_glyph: &mut context_menu_resolve_glyph,
            });
        }
    }

    fn prompt_tooltip_anchor(&self, prepared: &PreparedFrame) -> Option<(String, Rect)> {
        self.prompt_bar_hover.and_then(|(pane_id, hover)| {
            let pane = self.panes.get(&pane_id)?;
            let (_, pane_rect) = prepared.pane_rects.iter().find(|(id, _)| *id == pane_id)?;
            let full_text = prompt_bar::hovered_prompt_text(pane, hover)?;
            let prompt_bar_height = pane.prompt_bar_height(prepared.prompt_bar_cell_size.1, true);
            if prompt_bar_height <= 0.0 {
                return None;
            }
            let tab_bar_height = if pane.edges.top() {
                pane_tab_bar_h(
                    pane.workspace_id,
                    &prepared.ws_tab_bar_heights,
                    &prepared.ws_tab_bar_data,
                )
            } else {
                0.0
            };
            let bar_y = if prepared.prompt_bar_at_top {
                pane_rect.y + tab_bar_height
            } else {
                pane_rect.y + pane_rect.height - prompt_bar_height
            };
            let anchor = layout::Rect {
                x: pane_rect.x,
                y: bar_y,
                width: pane_rect.width,
                height: prompt_bar_height,
            };
            let effective_width = prompt_bar::prompt_bar_text_width(
                pane,
                anchor,
                prepared.prompt_bar_cell_size,
                hover,
            )?;
            prompt_bar::is_prompt_truncated(
                full_text,
                effective_width,
                prepared.prompt_bar_cell_size.0,
            )
            .then(|| (full_text.to_owned(), anchor))
        })
    }

    fn apply_prompt_tooltip_overlay(
        &mut self,
        prepared: &PreparedFrame,
        all_instances: &mut Vec<CellInstance>,
    ) {
        let Some((text, anchor)) = self.prompt_tooltip_anchor(prepared) else { return };
        let Some(gpu) = self.gpu.as_mut() else { return };
        let mut prompt_tooltip_resolve_glyph =
            |ch: char| gpu.renderer.resolve_glyph(&gpu.device, &gpu.queue, ch);
        tooltip::render_tooltip(tooltip::TooltipRenderContext {
            out: all_instances,
            text: &text,
            anchor,
            position: tooltip::TooltipPosition::Below,
            bg_color: [0.086, 0.086, 0.188, 1.0],
            fg_color: [0.690, 0.690, 0.816, 1.0],
            border_color: [0.149, 0.149, 0.282, 1.0],
            cell_size: prepared.cell_size,
            viewport_width: prepared.full_viewport.width,
            resolve_glyph: &mut prompt_tooltip_resolve_glyph,
        });
    }

    fn apply_active_tooltip_overlay(
        &mut self,
        prepared: &PreparedFrame,
        all_instances: &mut Vec<CellInstance>,
    ) {
        let Some((text, anchor, position)) = self.active_tooltip.clone() else { return };
        let Some(gpu) = self.gpu.as_mut() else { return };
        let mut active_tooltip_resolve_glyph =
            |ch: char| gpu.renderer.resolve_glyph(&gpu.device, &gpu.queue, ch);
        tooltip::render_tooltip(tooltip::TooltipRenderContext {
            out: all_instances,
            text: &text,
            anchor,
            position,
            bg_color: prepared.sb_colors.bg,
            fg_color: prepared.sb_colors.text,
            border_color: prepared.sb_colors.separator,
            cell_size: prepared.cell_size,
            viewport_width: prepared.full_viewport.width,
            resolve_glyph: &mut active_tooltip_resolve_glyph,
        });
    }

    fn apply_palette_or_search_overlay(
        &mut self,
        prepared: &PreparedFrame,
        all_instances: &mut Vec<CellInstance>,
    ) {
        let Some(gpu) = self.gpu.as_mut() else { return };
        if self.command_palette.is_active() {
            let labels: Vec<String> =
                self.command_palette_items.iter().map(|item| item.label.clone()).collect();
            let mut palette_resolve_glyph =
                |ch: char| gpu.renderer.resolve_glyph(&gpu.device, &gpu.queue, ch);
            self.command_palette.build_instances(command_palette::CommandPaletteBuildContext {
                out: all_instances,
                viewport: prepared.full_viewport,
                cell_size: prepared.cell_size,
                chrome: &self.theme.chrome,
                items: &labels,
                resolve_glyph: &mut palette_resolve_glyph,
            });
            return;
        }
        if self.search_overlay.is_active() {
            let mut search_overlay_resolve_glyph =
                |ch: char| gpu.renderer.resolve_glyph(&gpu.device, &gpu.queue, ch);
            self.search_overlay.build_instances(search_overlay::SearchOverlayBuildContext {
                out: all_instances,
                viewport: prepared.full_viewport,
                cell_size: prepared.cell_size,
                chrome: &self.theme.chrome,
                resolve_glyph: &mut search_overlay_resolve_glyph,
            });
        }
    }

    fn present_terminal_frame(
        &mut self,
        frame: wgpu::SurfaceTexture,
        view: &wgpu::TextureView,
        all_instances: &[CellInstance],
        presentation: TerminalFramePresentation,
    ) {
        let Some(gpu) = self.gpu.as_mut() else { return };
        gpu.renderer.pipeline_mut().update_instances(&gpu.device, &gpu.queue, all_instances);
        let mut encoder = gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("multi-pane encoder"),
        });
        let clear_color = {
            let mut color = gpu.renderer.default_bg();
            let [_, _, _, alpha] = &mut color;
            *alpha *= self.opacity;
            color
        };
        gpu.renderer.pipeline_mut().render_with_clear(&mut encoder, view, clear_color);
        gpu.queue.submit(std::iter::once(encoder.finish()));
        self.notify_pre_present();
        frame.present();
        if presentation.refresh_window_title {
            self.update_window_title();
        }
        if presentation.request_redraw {
            self.request_redraw();
        }
    }

    fn resolve_scrollbar_color(&self) -> [f32; 4] {
        self.config.appearance.scrollbar_color.as_ref().map_or(
            scribe_renderer::srgb_to_linear_rgba(self.theme.chrome.scrollbar),
            |hex| {
                scribe_common::theme::hex_to_rgba(hex).map_or(
                    scribe_renderer::srgb_to_linear_rgba(self.theme.chrome.scrollbar),
                    |color| {
                        let linear = scribe_renderer::srgb_to_linear_rgba(color);
                        [
                            linear.first().copied().unwrap_or(0.0),
                            linear.get(1).copied().unwrap_or(0.0),
                            linear.get(2).copied().unwrap_or(0.0),
                            0.4,
                        ]
                    },
                )
            },
        )
    }

    fn resolve_prompt_bar_colors(&self) -> prompt_bar::PromptBarColors {
        let chrome = &self.theme.chrome;
        let resolve = |opt: &Option<String>, fallback: [f32; 4]| -> [f32; 4] {
            opt.as_deref().and_then(|hex| scribe_common::theme::hex_to_rgba(hex).ok()).map_or(
                scribe_renderer::srgb_to_linear_rgba(fallback),
                scribe_renderer::srgb_to_linear_rgba,
            )
        };
        prompt_bar::PromptBarColors {
            first_row_bg: resolve(
                &self.config.appearance.prompt_bar_first_row_bg,
                chrome.prompt_bar_first_row_bg,
            ),
            second_row_bg: resolve(
                &self.config.appearance.prompt_bar_second_row_bg,
                chrome.prompt_bar_second_row_bg,
            ),
            text: resolve(&self.config.appearance.prompt_bar_text, chrome.prompt_bar_text),
            icon_first: resolve(
                &self.config.appearance.prompt_bar_icon_first,
                chrome.prompt_bar_icon_first,
            ),
            icon_latest: resolve(
                &self.config.appearance.prompt_bar_icon_latest,
                chrome.prompt_bar_icon_latest,
            ),
        }
    }

    /// Reconfigure the surface and renderer on window resize.
    fn handle_resize(&mut self, size: winit::dpi::PhysicalSize<u32>) {
        if size.width == 0 || size.height == 0 {
            return;
        }
        tracing::info!(w = size.width, h = size.height, "handle_resize");

        // Detect scale-factor changes (e.g. moving between Retina and non-Retina
        // displays). Winit 0.30 folds scale-factor changes into Resized events.
        let new_scale =
            self.window.as_ref().map_or(self.scale_factor, |w| w.scale_factor().cast::<f32>());
        let scale_changed = (new_scale - self.scale_factor).abs() > f32::EPSILON;
        if scale_changed {
            tracing::info!(old = self.scale_factor, new = new_scale, "scale factor changed");
            self.scale_factor = new_scale;
        }

        let Some(gpu) = &mut self.gpu else { return };

        gpu.surface_config.width = size.width;
        gpu.surface_config.height = size.height;
        gpu.surface.configure(&gpu.device, &gpu.surface_config);

        // Rebuild the glyph atlas when the scale factor changes so glyphs
        // render at the correct physical resolution.
        if scale_changed {
            let font_size =
                (self.config.appearance.font_size + f32::from(self.zoom_level)) * self.scale_factor;
            let params = scribe_renderer::atlas::FontParams {
                family: self.config.appearance.font.clone(),
                size: font_size.max(6.0),
                weight: self.config.appearance.font_weight,
                weight_bold: self.config.appearance.font_weight_bold,
                ligatures: self.config.appearance.ligatures,
                line_padding: self.config.appearance.line_padding,
            };
            gpu.renderer.rebuild_atlas(&gpu.device, &gpu.queue, &params);
        }

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

    /// Returns `true` when a compositor overlay (e.g. screenshot tool) is
    /// covering the window — key events should be suppressed.
    fn compositor_overlay_active(&mut self) -> bool {
        #[cfg(target_os = "linux")]
        if let Some(guard) = &mut self.x11_focus_guard {
            return guard.should_suppress_key();
        }
        false
    }

    /// Translate a keyboard event and forward it to the correct handler.
    fn handle_keyboard(&mut self, event: &winit::event::KeyEvent) {
        // Reset cursor blink on any key-down so the cursor never stays
        // hidden mid-blink, regardless of which handler consumes the event.
        if event.state == winit::event::ElementState::Pressed {
            self.cursor.visible = true;
            self.blink_timer = Instant::now();
        }

        // Dismiss context menu on any key press.
        if self.context_menu.is_some()
            && event.state == winit::event::ElementState::Pressed
            && event.logical_key == Key::Named(NamedKey::Escape)
        {
            self.context_menu = None;
            self.request_redraw();
            return;
        }

        if self.handle_command_palette_keyboard(event) || self.handle_search_overlay_keyboard(event)
        {
            return;
        }

        let Some(action) = input::translate_key_action(event, self.modifiers, &self.bindings)
        else {
            return;
        };

        match action {
            KeyAction::Terminal(bytes) => self.handle_terminal_key(bytes),
            KeyAction::Layout(layout_action) => self.handle_layout_action(layout_action),
            KeyAction::OpenCommandPalette => self.handle_open_command_palette(),
            KeyAction::OpenSettings => open_or_focus_settings(),
            KeyAction::OpenFind => self.handle_open_find(),
        }
    }

    fn handle_terminal_key(&mut self, bytes: Vec<u8>) {
        let Some(tx) = self.cmd_tx.clone() else { return };
        let focused_pane_id = {
            let Some(tab) = self.window_layout.active_tab() else { return };
            tab.focused_pane
        };
        // Send any pending resize first so SIGWINCH lands before the bytes do.
        self.flush_resize_if_pending();
        let Some(pane) = self.panes.get_mut(&focused_pane_id) else { return };
        let sid = pane.session_id;

        let scrolled_up = pane.term.grid().display_offset() > 0;

        // When scroll_pin is active in an AI pane, only snap to bottom on
        // Enter — other keystrokes are sent without scrolling so the user
        // can compose prompts while reading scrollback.
        let is_enter = bytes == b"\r";
        let split_scroll_eligibility =
            SplitScrollEligibility::for_session(sid, &self.ai_tracker, &self.config.terminal);
        let pin_active = split_scroll_eligible(pane, split_scroll_eligibility);

        if scrolled_up && (!pin_active || is_enter) {
            pane.split_scroll = None;
            pane.term.scroll_display(alacritty_terminal::grid::Scroll::Bottom);
            pane.scrollbar_state.on_scroll_action();
            pane.content_dirty = true;
            self.ensure_animation_running();
        } else if pin_active && !is_enter {
            // Activate split-scroll if not already active.
            if pane.split_scroll.is_none() {
                pane.split_scroll = Some(split_scroll::SplitScrollState::new());
            }
            pane.content_dirty = true;
        }

        if tx
            .send(ClientCommand::KeyInput {
                session_id: sid,
                data: bytes,
                dismisses_attention: true,
            })
            .is_err()
        {
            tracing::warn!("IPC channel closed; keyboard input dropped");
        }

        // Clear "waiting for input / permission" indicators on real keystrokes.
        if self
            .ai_tracker
            .provider_for_session(sid)
            .is_some_and(|provider| self.config.terminal.ai_provider_enabled(provider))
        {
            self.ai_tracker.clear_attention_states(sid);
        }
    }

    fn handle_layout_action(&mut self, action: LayoutAction) {
        if self.handle_pane_layout_action(action)
            || self.handle_workspace_layout_action(action)
            || self.handle_tab_layout_action(action)
            || self.handle_clipboard_layout_action(action)
            || self.handle_navigation_layout_action(action)
            || self.handle_view_layout_action(action)
        {}
    }

    fn handle_pane_layout_action(&mut self, action: LayoutAction) -> bool {
        match action {
            LayoutAction::SplitVertical => self.handle_split(layout::SplitDirection::Horizontal),
            LayoutAction::SplitHorizontal => self.handle_split(layout::SplitDirection::Vertical),
            LayoutAction::ClosePane => self.handle_close_pane(),
            LayoutAction::FocusNext => self.handle_focus_next(),
            LayoutAction::FocusLeft => self.handle_focus_directional(layout::FocusDirection::Left),
            LayoutAction::FocusRight => {
                self.handle_focus_directional(layout::FocusDirection::Right);
            }
            LayoutAction::FocusUp => self.handle_focus_directional(layout::FocusDirection::Up),
            LayoutAction::FocusDown => self.handle_focus_directional(layout::FocusDirection::Down),
            _ => return false,
        }
        true
    }

    fn handle_workspace_layout_action(&mut self, action: LayoutAction) -> bool {
        match action {
            LayoutAction::WorkspaceSplitVertical => {
                self.handle_workspace_split(layout::SplitDirection::Horizontal);
            }
            LayoutAction::WorkspaceSplitHorizontal => {
                self.handle_workspace_split(layout::SplitDirection::Vertical);
            }
            LayoutAction::WorkspaceFocusLeft => {
                self.handle_workspace_focus_directional(layout::FocusDirection::Left);
            }
            LayoutAction::WorkspaceFocusRight => {
                self.handle_workspace_focus_directional(layout::FocusDirection::Right);
            }
            LayoutAction::WorkspaceFocusUp => {
                self.handle_workspace_focus_directional(layout::FocusDirection::Up);
            }
            LayoutAction::WorkspaceFocusDown => {
                self.handle_workspace_focus_directional(layout::FocusDirection::Down);
            }
            LayoutAction::NewWindow => self.handle_new_window(),
            _ => return false,
        }
        true
    }

    fn handle_tab_layout_action(&mut self, action: LayoutAction) -> bool {
        match action {
            LayoutAction::NewTab => self.handle_new_tab(),
            LayoutAction::NewClaudeTab => self.handle_new_claude_tab(),
            LayoutAction::NewClaudeResumeTab => self.handle_new_claude_resume_tab(),
            LayoutAction::NewCodexTab => self.handle_new_codex_tab(),
            LayoutAction::NewCodexResumeTab => self.handle_new_codex_resume_tab(),
            LayoutAction::CloseTab => self.handle_close_tab(),
            LayoutAction::NextTab => self.handle_next_tab(),
            LayoutAction::PrevTab => self.handle_prev_tab(),
            LayoutAction::SelectTab(idx) => self.handle_select_tab(idx),
            _ => return false,
        }
        true
    }

    fn handle_clipboard_layout_action(&mut self, action: LayoutAction) -> bool {
        match action {
            LayoutAction::CopySelection => self.perform_copy(),
            LayoutAction::PasteClipboard => self.perform_paste(),
            _ => return false,
        }
        true
    }

    fn handle_navigation_layout_action(&mut self, action: LayoutAction) -> bool {
        match action {
            LayoutAction::ScrollUp => self.handle_scroll_up(),
            LayoutAction::ScrollDown => self.handle_scroll_down(),
            LayoutAction::ScrollTop => self.handle_scroll_top(),
            LayoutAction::ScrollBottom => self.handle_scroll_bottom(),
            LayoutAction::PromptJumpUp => self.handle_prompt_jump_up(),
            LayoutAction::PromptJumpDown => self.handle_prompt_jump_down(),
            _ => return false,
        }
        true
    }

    fn handle_view_layout_action(&mut self, action: LayoutAction) -> bool {
        match action {
            LayoutAction::ZoomIn => self.zoom_step(1),
            LayoutAction::ZoomOut => self.zoom_step(-1),
            LayoutAction::ZoomReset => self.zoom_reset(),
            _ => return false,
        }
        true
    }

    fn execute_automation_action(&mut self, action: AutomationAction) {
        match action {
            AutomationAction::OpenSettings => open_or_focus_settings(),
            AutomationAction::OpenFind => self.handle_open_find(),
            AutomationAction::NewTab => self.handle_new_tab(),
            AutomationAction::NewClaudeTab => self.handle_new_claude_tab(),
            AutomationAction::NewClaudeResumeTab => self.handle_new_claude_resume_tab(),
            AutomationAction::NewCodexTab => self.handle_new_codex_tab(),
            AutomationAction::NewCodexResumeTab => self.handle_new_codex_resume_tab(),
            AutomationAction::SplitVertical => {
                self.handle_layout_action(LayoutAction::SplitVertical);
            }
            AutomationAction::SplitHorizontal => {
                self.handle_layout_action(LayoutAction::SplitHorizontal);
            }
            AutomationAction::ClosePane => self.handle_layout_action(LayoutAction::ClosePane),
            AutomationAction::CloseTab => self.handle_layout_action(LayoutAction::CloseTab),
            AutomationAction::NewWindow => self.handle_layout_action(LayoutAction::NewWindow),
            AutomationAction::SwitchProfile { name } => {
                match scribe_common::profiles::switch_profile(&name) {
                    Ok(_) => self.handle_config_changed(),
                    Err(e) => {
                        tracing::warn!(
                            profile = %name,
                            error = %e,
                            "failed to switch profile"
                        );
                    }
                }
            }
            AutomationAction::OpenUpdateDialog => self.open_update_dialog(),
            AutomationAction::FocusSession { session_id } => {
                self.handle_focus_session(session_id);
            }
        }
    }

    /// Raise the OS window and switch to the tab containing the given session.
    fn handle_focus_session(&mut self, session_id: SessionId) {
        // Consume any pending notification focus to prevent the
        // Focused(true) handler from re-dispatching the same switch.
        let _ = self.notification_tracker.take_pending_focus();

        let Some(&pane_id) = self.session_to_pane.get(&session_id) else { return };
        let Some(pane) = self.panes.get(&pane_id) else { return };
        let ws_id = pane.workspace_id;

        let Some(ws) = self.window_layout.find_workspace(ws_id) else { return };
        // Search all tabs — not just root session_id — because the session
        // may be a split pane within a tab rather than the tab root.
        let Some(tab_idx) = ws.tabs.iter().position(|t| {
            t.session_id == session_id
                || t.pane_layout
                    .all_pane_ids()
                    .iter()
                    .any(|&pid| self.panes.get(&pid).is_some_and(|p| p.session_id == session_id))
        }) else {
            return;
        };

        let old_session = self.focused_session_id();
        self.window_layout.set_focused_workspace(ws_id);
        self.switch_active_tab(ws_id, tab_idx);
        let new_session = self.focused_session_id();
        if old_session != new_session {
            self.notify_focus_change(new_session, old_session);
        }

        if let Some(window) = &self.window {
            window.focus_window();
            window.request_user_attention(None);
        }
        self.request_redraw();
    }

    fn handle_split(&mut self, direction: layout::SplitDirection) {
        let Some(plan) = self.prepare_split_pane(direction) else { return };
        self.finish_split_pane(plan);
    }

    fn prepare_split_pane(&mut self, direction: layout::SplitDirection) -> Option<SplitPanePlan> {
        let focused = self.window_layout.active_tab()?.focused_pane;
        let inherited_cwd = self.panes.get(&focused).and_then(|pane| pane.cwd.clone());
        let workspace_id = self.window_layout.focused_workspace_id();

        let new_pane_id =
            self.window_layout.active_tab_mut()?.pane_layout.split_pane(focused, direction)?;
        if let Some(active) = self.window_layout.active_tab_mut() {
            active.pane_layout.equalize_all_ratios();
        }

        let gpu = self.gpu.as_ref()?;
        let ws_viewport = workspace_viewport(
            &gpu.surface_config,
            self.config.appearance.status_bar_height * self.scale_factor,
        );
        let ws_rects = self.window_layout.compute_workspace_rects(ws_viewport);
        let ws_rect =
            ws_rects.iter().find(|(wid, _)| *wid == workspace_id).map_or(ws_viewport, |(_, r)| *r);
        let rects = self.window_layout.active_tab()?.pane_layout.compute_rects(ws_rect);
        let (new_rect, new_edges) = rects
            .iter()
            .find(|(id, _, _)| *id == new_pane_id)
            .map_or((ws_rect, PaneEdges::all_external()), |&(_, rect, edges)| (rect, edges));
        let tab_bar_h = self.tab_bar_height_for(workspace_id, ws_rect);
        let grid = pane::compute_pane_grid(&pane::PaneGridRequest {
            rect: new_rect,
            cell_size: (gpu.renderer.cell_size().width, gpu.renderer.cell_size().height),
            tab_bar_height: if new_edges.top() { tab_bar_h } else { 0.0 },
            prompt_bar_height: 0.0,
            padding: &pane::effective_padding(
                &self.config.appearance.content_padding,
                new_edges,
                self.scale_factor,
            ),
        });

        Some(SplitPanePlan {
            workspace_id,
            new_pane_id,
            session_id: SessionId::new(),
            inherited_cwd,
            rects,
            ws_rects,
            new_rect,
            new_edges,
            grid,
        })
    }

    fn finish_split_pane(&mut self, plan: SplitPanePlan) {
        let pane = Pane::new(
            pane::PaneLayoutState { rect: plan.new_rect, grid: plan.grid, edges: plan.new_edges },
            plan.session_id,
            plan.workspace_id,
            restore_state::LaunchBinding {
                launch_id: SessionId::new().to_full_string(),
                kind: restore_state::LaunchKind::Shell,
                fallback_cwd: plan.inherited_cwd.clone(),
            },
        );

        self.panes.insert(plan.new_pane_id, pane);
        self.url_caches.insert(plan.new_pane_id, url_detect::PaneUrlCache::new());
        self.session_to_pane.insert(plan.session_id, plan.new_pane_id);
        self.pending_sessions.push_back(plan.session_id);

        if let Some(tx) = &self.cmd_tx {
            send_command(
                tx,
                ClientCommand::CreateSession {
                    workspace_id: plan.workspace_id,
                    split_direction: None,
                    cwd: plan.inherited_cwd,
                    size: self.terminal_size_for_grid(plan.grid),
                    command: None,
                },
            );
        }

        self.resize_all_panes_from_rects(&plan.rects, &plan.ws_rects);
        if let Some(active) = self.window_layout.active_tab_mut() {
            active.focused_pane = plan.new_pane_id;
        }
        self.request_redraw();
    }

    fn handle_workspace_split(&mut self, direction: layout::SplitDirection) {
        let Some(gpu) = &self.gpu else { return };
        let accent = Some(self.theme.chrome.accent);
        let ws_viewport = workspace_viewport(
            &gpu.surface_config,
            self.config.appearance.status_bar_height * self.scale_factor,
        );
        let cell = gpu.renderer.cell_size();

        // Split the window layout tree, creating a new workspace region.
        let Some(new_workspace_id) = self.window_layout.split_workspace(direction, accent) else {
            return;
        };

        self.window_layout.equalize_all_workspace_ratios();

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
        let grid = pane::compute_pane_grid(&pane::PaneGridRequest {
            rect: ws_rect,
            cell_size: (cell.width, cell.height),
            tab_bar_height: tab_bar_h,
            prompt_bar_height: 0.0,
            padding: &self.config.appearance.content_padding,
        });
        let pane = Pane::new(
            pane::PaneLayoutState { rect: ws_rect, grid, edges: PaneEdges::all_external() },
            session_id,
            new_workspace_id,
            restore_state::LaunchBinding {
                launch_id: SessionId::new().to_full_string(),
                kind: restore_state::LaunchKind::Shell,
                fallback_cwd: None,
            },
        );

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
                    size: self.terminal_size_for_grid(grid),
                    command: None,
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
        self.report_workspace_tree();
        self.request_redraw();
    }

    /// Return the session ID of the currently focused pane, if any.
    fn focused_session_id(&self) -> Option<SessionId> {
        let tab = self.window_layout.active_tab()?;
        let pane = self.panes.get(&tab.focused_pane)?;
        Some(pane.session_id)
    }

    /// Send a focus-change notification to the server so it can relay
    /// CSI focus events (`\x1b[I` / `\x1b[O`) to PTY applications.
    fn notify_focus_change(&self, gained: Option<SessionId>, lost: Option<SessionId>) {
        if gained.is_none() && lost.is_none() {
            return;
        }
        if let Some(tx) = &self.cmd_tx {
            send_command(tx, ipc_client::ClientCommand::FocusChanged { gained, lost });
        }
    }

    fn handle_focus_next(&mut self) {
        let old_session = self.focused_session_id();
        let Some(active) = self.window_layout.active_tab_mut() else { return };
        let current = active.focused_pane;
        active.focused_pane = active.pane_layout.next_pane(current);
        tracing::debug!(from = %current, to = %active.focused_pane, "focus cycled");
        let new_session = self.focused_session_id();
        if old_session != new_session {
            self.notify_focus_change(new_session, old_session);
        }
        self.request_redraw();
    }

    fn handle_workspace_focus_directional(&mut self, direction: layout::FocusDirection) {
        let Some(gpu) = &self.gpu else { return };
        let ws_viewport = workspace_viewport(
            &gpu.surface_config,
            self.config.appearance.status_bar_height * self.scale_factor,
        );
        let old_session = self.focused_session_id();
        let Some(target) = self.window_layout.find_workspace_in_direction(direction, ws_viewport)
        else {
            return;
        };
        self.window_layout.set_focused_workspace(target);
        let new_session = self.focused_session_id();
        if old_session != new_session {
            self.notify_focus_change(new_session, old_session);
        }
        self.request_redraw();
    }

    // -----------------------------------------------------------------------
    // Directional pane focus
    // -----------------------------------------------------------------------

    fn handle_focus_directional(&mut self, direction: layout::FocusDirection) {
        let Some(gpu) = &self.gpu else { return };
        let ws_viewport = workspace_viewport(
            &gpu.surface_config,
            self.config.appearance.status_bar_height * self.scale_factor,
        );
        let ws_id = self.window_layout.focused_workspace_id();
        let ws_rects = self.window_layout.compute_workspace_rects(ws_viewport);
        let ws_rect =
            ws_rects.iter().find(|(wid, _)| *wid == ws_id).map_or(ws_viewport, |(_, r)| *r);

        let Some(active) = self.window_layout.active_tab() else { return };
        let current = active.focused_pane;
        let rects = active.pane_layout.compute_rects(ws_rect);

        let Some(target) = active.pane_layout.find_pane_in_direction(current, direction, &rects)
        else {
            return;
        };
        let old_session = self.focused_session_id();
        let Some(active_mut) = self.window_layout.active_tab_mut() else { return };
        active_mut.focused_pane = target;
        let new_session = self.focused_session_id();
        if old_session != new_session {
            self.notify_focus_change(new_session, old_session);
        }
        self.request_redraw();
    }

    // -----------------------------------------------------------------------
    // Tab management
    // -----------------------------------------------------------------------

    fn handle_new_tab(&mut self) {
        self.create_new_tab(None, None);
    }

    fn ai_tab_command(provider: AiProvider, resume: bool) -> Vec<String> {
        let shell = scribe_common::shell::default_shell_program();
        let command = match (provider, resume) {
            (AiProvider::ClaudeCode, false) => String::from("exec claude"),
            (AiProvider::ClaudeCode, true) => String::from("exec claude --resume"),
            (AiProvider::CodexCode, false) => String::from("exec codex"),
            (AiProvider::CodexCode, true) => String::from("exec codex resume"),
        };
        vec![shell, String::from("-lic"), command]
    }

    /// Return the focused workspace's project root, if the workspace is named
    /// (i.e. the CWD matched a configured workspace root).
    fn focused_workspace_project_root(&self) -> Option<std::path::PathBuf> {
        self.window_layout.focused_workspace().and_then(|ws| ws.project_root.clone())
    }

    fn handle_new_claude_tab(&mut self) {
        // Wrap in an interactive login shell so the user's full environment
        // is initialised before the selected AI CLI starts. The server runs
        // with a minimal service environment, so `-l`/`-i` ensure PATH
        // additions from shell startup files are available before `exec`.
        //
        // When inside a workspace, start at the project root rather than
        // inheriting the current tab's CWD.
        let project_root = self.focused_workspace_project_root();
        self.create_new_tab(
            Some(Self::ai_tab_command(AiProvider::ClaudeCode, false)),
            project_root,
        );
    }

    fn handle_new_claude_resume_tab(&mut self) {
        let project_root = self.focused_workspace_project_root();
        self.create_new_tab(Some(Self::ai_tab_command(AiProvider::ClaudeCode, true)), project_root);
    }

    fn handle_new_codex_tab(&mut self) {
        let project_root = self.focused_workspace_project_root();
        self.create_new_tab(Some(Self::ai_tab_command(AiProvider::CodexCode, false)), project_root);
    }

    fn handle_new_codex_resume_tab(&mut self) {
        let project_root = self.focused_workspace_project_root();
        self.create_new_tab(Some(Self::ai_tab_command(AiProvider::CodexCode, true)), project_root);
    }

    fn launch_binding_for_command(
        command: Option<&Vec<String>>,
        inherited_cwd: Option<std::path::PathBuf>,
    ) -> restore_state::LaunchBinding {
        match command {
            None => restore_replay::new_shell_binding(inherited_cwd),
            Some(argv) if restore_replay::is_ai_command(argv, AiProvider::ClaudeCode, true) => {
                restore_replay::new_ai_binding(
                    AiProvider::ClaudeCode,
                    restore_state::AiResumeMode::Resume,
                    inherited_cwd,
                    None,
                )
            }
            Some(argv) if restore_replay::is_ai_command(argv, AiProvider::CodexCode, true) => {
                restore_replay::new_ai_binding(
                    AiProvider::CodexCode,
                    restore_state::AiResumeMode::Resume,
                    inherited_cwd,
                    None,
                )
            }
            Some(argv) if restore_replay::is_ai_command(argv, AiProvider::ClaudeCode, false) => {
                restore_replay::new_ai_binding(
                    AiProvider::ClaudeCode,
                    restore_state::AiResumeMode::New,
                    inherited_cwd,
                    None,
                )
            }
            Some(argv) if restore_replay::is_ai_command(argv, AiProvider::CodexCode, false) => {
                restore_replay::new_ai_binding(
                    AiProvider::CodexCode,
                    restore_state::AiResumeMode::New,
                    inherited_cwd,
                    None,
                )
            }
            Some(argv) => restore_replay::new_custom_binding(argv.clone(), inherited_cwd),
        }
    }
    fn create_new_tab(
        &mut self,
        command: Option<Vec<String>>,
        cwd_override: Option<std::path::PathBuf>,
    ) {
        // Use the override if provided (e.g. workspace project root for AI
        // tabs), otherwise inherit the focused pane's CWD.
        let inherited_cwd = cwd_override.or_else(|| {
            self.window_layout
                .active_tab()
                .and_then(|t| self.panes.get(&t.focused_pane))
                .and_then(|p| p.cwd.clone())
        });

        let workspace_id = self.window_layout.focused_workspace_id();
        let session_id = SessionId::new();

        let Some(pane_id) = self.window_layout.add_tab(workspace_id, session_id) else { return };

        let Some(gpu) = &self.gpu else { return };
        let ws_viewport = workspace_viewport(
            &gpu.surface_config,
            self.config.appearance.status_bar_height * self.scale_factor,
        );
        let cell = gpu.renderer.cell_size();

        let ws_rects = self.window_layout.compute_workspace_rects(ws_viewport);
        let ws_rect =
            ws_rects.iter().find(|(wid, _)| *wid == workspace_id).map_or(ws_viewport, |(_, r)| *r);

        let tab_bar_h = self.tab_bar_height_for(workspace_id, ws_rect);
        let grid = pane::compute_pane_grid(&pane::PaneGridRequest {
            rect: ws_rect,
            cell_size: (cell.width, cell.height),
            tab_bar_height: tab_bar_h,
            prompt_bar_height: 0.0,
            padding: &self.config.appearance.content_padding,
        });
        let pane = Pane::new(
            pane::PaneLayoutState { rect: ws_rect, grid, edges: PaneEdges::all_external() },
            session_id,
            workspace_id,
            Self::launch_binding_for_command(command.as_ref(), inherited_cwd.clone()),
        );

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
                    size: self.terminal_size_for_grid(grid),
                    command,
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
        self.line_drag_anchor = None;
        self.mouse_click.reset();
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
        let old_session = self.focused_session_id();
        if self.switch_active_tab(ws_id, next_idx) {
            let new_session = self.focused_session_id();
            if old_session != new_session {
                self.notify_focus_change(new_session, old_session);
            }
            self.request_redraw();
        }
    }

    fn handle_prev_tab(&mut self) {
        let ws_id = self.window_layout.focused_workspace_id();
        let Some(ws) = self.window_layout.focused_workspace() else { return };
        let prev_idx = ws.prev_tab_index();
        let old_session = self.focused_session_id();
        if self.switch_active_tab(ws_id, prev_idx) {
            let new_session = self.focused_session_id();
            if old_session != new_session {
                self.notify_focus_change(new_session, old_session);
            }
            self.request_redraw();
        }
    }

    fn handle_select_tab(&mut self, index: usize) {
        let ws_id = self.window_layout.focused_workspace_id();
        let old_session = self.focused_session_id();
        if self.switch_active_tab(ws_id, index) {
            let new_session = self.focused_session_id();
            if old_session != new_session {
                self.notify_focus_change(new_session, old_session);
            }
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
        let (raw, cleanup_active) = {
            let Some(tab) = self.window_layout.active_tab() else {
                return;
            };
            let Some(pane) = self.panes.get(&tab.focused_pane) else {
                return;
            };
            let text = selection::extract_text(&pane.term, &sel);
            let cleanup_active = self.ai_tracker.provider_for_session(pane.session_id).is_some();
            (text, cleanup_active)
        };

        if raw.is_empty() {
            return;
        }

        let text = clipboard_cleanup::prepare_copy_text(
            &raw,
            clipboard_cleanup::CopyTextOptions {
                ai_session_active: cleanup_active,
                cleanup_enabled: self.config.terminal.clipboard.claude_copy_cleanup,
            },
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
        self.send_paste_data(&text);
    }

    /// Send paste text to the focused pane, wrapping in bracketed-paste
    /// sequences when the terminal has enabled that mode.
    ///
    /// Large pastes are split into chunks that each fit within the server's
    /// 4 KiB `KeyInput` limit. Bracketed-paste start/end markers are placed
    /// on the first and last chunks only so the shell sees one contiguous
    /// paste region.
    fn send_paste_data(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.flush_resize_if_pending();

        let Some(tx) = self.cmd_tx.clone() else { return };
        let Some(target) = self.prepare_paste_target() else { return };
        let raw = text.as_bytes();

        if Self::try_send_single_paste(&tx, &target, raw) {
            return;
        }

        Self::send_chunked_paste(&tx, &target, raw);
    }

    fn prepare_paste_target(&mut self) -> Option<PasteTarget> {
        let focused_pane_id = self.window_layout.active_tab()?.focused_pane;
        let scrolled_up = {
            let pane = self.panes.get_mut(&focused_pane_id)?;
            let offset = pane.term.grid().display_offset();
            if offset > 0 {
                pane.split_scroll = None;
                pane.term.scroll_display(alacritty_terminal::grid::Scroll::Bottom);
                pane.scrollbar_state.on_scroll_action();
                pane.content_dirty = true;
            }
            offset > 0
        };
        if scrolled_up {
            self.ensure_animation_running();
        }

        let pane = self.panes.get(&focused_pane_id)?;
        Some(PasteTarget {
            session_id: pane.session_id,
            bracketed: pane
                .term
                .mode()
                .contains(alacritty_terminal::term::TermMode::BRACKETED_PASTE),
        })
    }

    fn try_send_single_paste(tx: &Sender<ClientCommand>, target: &PasteTarget, raw: &[u8]) -> bool {
        if !target.bracketed && raw.len() <= MAX_KEY_INPUT_CHUNK {
            return Self::send_paste_message(tx, target.session_id, raw.to_vec());
        }
        if target.bracketed
            && raw.len() + BRACKETED_PASTE_START.len() + BRACKETED_PASTE_END.len()
                <= MAX_KEY_INPUT_CHUNK
        {
            let mut buf = Vec::with_capacity(
                BRACKETED_PASTE_START.len() + raw.len() + BRACKETED_PASTE_END.len(),
            );
            buf.extend_from_slice(BRACKETED_PASTE_START);
            buf.extend_from_slice(raw);
            buf.extend_from_slice(BRACKETED_PASTE_END);
            return Self::send_paste_message(tx, target.session_id, buf);
        }
        false
    }

    fn send_chunked_paste(tx: &Sender<ClientCommand>, target: &PasteTarget, raw: &[u8]) {
        let mut offset = 0;
        let mut first = true;
        while offset < raw.len() {
            let remaining = raw.len() - offset;
            let is_last = |payload_len: usize| offset + payload_len >= raw.len();
            let mut budget = MAX_KEY_INPUT_CHUNK;
            if first && target.bracketed {
                budget -= BRACKETED_PASTE_START.len();
            }
            if is_last(budget) && target.bracketed {
                budget = budget.saturating_sub(BRACKETED_PASTE_END.len());
            }
            let payload_len = remaining.min(budget);

            let mut chunk = Vec::with_capacity(MAX_KEY_INPUT_CHUNK);
            if first && target.bracketed {
                chunk.extend_from_slice(BRACKETED_PASTE_START);
            }
            if let Some(slice) = raw.get(offset..offset + payload_len) {
                chunk.extend_from_slice(slice);
            }
            if is_last(payload_len) && target.bracketed {
                chunk.extend_from_slice(BRACKETED_PASTE_END);
            }

            if !Self::send_paste_message(tx, target.session_id, chunk) {
                return;
            }
            offset += payload_len;
            first = false;
        }
    }

    fn send_paste_message(
        tx: &Sender<ClientCommand>,
        session_id: SessionId,
        data: Vec<u8>,
    ) -> bool {
        if tx
            .send(ClientCommand::KeyInput { session_id, data, dismisses_attention: false })
            .is_err()
        {
            tracing::warn!("IPC channel closed; paste dropped");
            return false;
        }
        true
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
        update_split_scroll(
            pane,
            self.config.terminal.scroll.scroll_pin,
            &self.ai_tracker,
            &self.config.terminal,
        );
        self.ensure_animation_running();
        self.request_redraw();
    }

    fn handle_scroll_down(&mut self) {
        let Some(tab) = self.window_layout.active_tab() else { return };
        let Some(pane) = self.panes.get_mut(&tab.focused_pane) else { return };
        pane.term.scroll_display(alacritty_terminal::grid::Scroll::PageDown);
        pane.scrollbar_state.on_scroll_action();
        pane.content_dirty = true;
        update_split_scroll(
            pane,
            self.config.terminal.scroll.scroll_pin,
            &self.ai_tracker,
            &self.config.terminal,
        );
        self.ensure_animation_running();
        self.request_redraw();
    }

    fn handle_scroll_top(&mut self) {
        let Some(tab) = self.window_layout.active_tab() else { return };
        let Some(pane) = self.panes.get_mut(&tab.focused_pane) else { return };
        pane.term.scroll_display(alacritty_terminal::grid::Scroll::Top);
        pane.scrollbar_state.on_scroll_action();
        pane.content_dirty = true;
        update_split_scroll(
            pane,
            self.config.terminal.scroll.scroll_pin,
            &self.ai_tracker,
            &self.config.terminal,
        );
        self.ensure_animation_running();
        self.request_redraw();
    }

    fn handle_scroll_bottom(&mut self) {
        let Some(tab) = self.window_layout.active_tab() else { return };
        let Some(pane) = self.panes.get_mut(&tab.focused_pane) else { return };
        pane.split_scroll = None;
        pane.term.scroll_display(alacritty_terminal::grid::Scroll::Bottom);
        pane.scrollbar_state.on_scroll_action();
        pane.content_dirty = true;
        self.ensure_animation_running();
        self.request_redraw();
    }

    fn handle_prompt_jump_up(&mut self) {
        let Some(tab) = self.window_layout.active_tab() else { return };
        let Some(pane) = self.panes.get_mut(&tab.focused_pane) else { return };
        let history = pane.term.grid().history_size();
        let offset = pane.term.grid().display_offset();
        let viewport_top_abs = history.saturating_sub(offset);
        let target = pane.prompt_marks.iter().rev().find(|&&mark| mark < viewport_top_abs).copied();
        if let Some(mark_pos) = target {
            let new_offset = history.saturating_sub(mark_pos);
            let delta = history_size_delta(new_offset, offset);
            if delta != 0 {
                pane.term.scroll_display(alacritty_terminal::grid::Scroll::Delta(delta));
                pane.scrollbar_state.on_scroll_action();
                pane.content_dirty = true;
                update_split_scroll(
                    pane,
                    self.config.terminal.scroll.scroll_pin,
                    &self.ai_tracker,
                    &self.config.terminal,
                );
                self.ensure_animation_running();
                self.request_redraw();
            }
        }
    }

    fn handle_prompt_jump_down(&mut self) {
        let Some(tab) = self.window_layout.active_tab() else { return };
        let Some(pane) = self.panes.get_mut(&tab.focused_pane) else { return };
        let history = pane.term.grid().history_size();
        let offset = pane.term.grid().display_offset();
        let viewport_top_abs = history.saturating_sub(offset);
        let target = pane.prompt_marks.iter().find(|&&mark| mark > viewport_top_abs).copied();
        if let Some(mark_pos) = target {
            let new_offset = history.saturating_sub(mark_pos);
            let delta = history_size_delta(new_offset, offset);
            if delta != 0 {
                pane.term.scroll_display(alacritty_terminal::grid::Scroll::Delta(delta));
                pane.scrollbar_state.on_scroll_action();
                pane.content_dirty = true;
                update_split_scroll(
                    pane,
                    self.config.terminal.scroll.scroll_pin,
                    &self.ai_tracker,
                    &self.config.terminal,
                );
                self.ensure_animation_running();
                self.request_redraw();
            }
        }
    }

    fn handle_mouse_wheel(&mut self, delta: winit::event::MouseScrollDelta) {
        use alacritty_terminal::term::TermMode;
        let natural = self.config.terminal.scroll.natural_scroll;
        let raw_lines = match delta {
            winit::event::MouseScrollDelta::LineDelta(_, y) => {
                // 3 terminal lines per scroll tick.
                round_f32_to_i32(y * 3.0)
            }
            winit::event::MouseScrollDelta::PixelDelta(pos) => {
                let Some(gpu) = &self.gpu else { return };
                let cell_h = gpu.renderer.cell_size().height;
                if cell_h <= 0.0 {
                    return;
                }
                let y = pos.y.cast::<f32>();
                round_f32_to_i32((y / cell_h).round())
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
            let count = usize::try_from(lines.unsigned_abs()).unwrap_or(usize::MAX);
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
        update_split_scroll(
            pane,
            self.config.terminal.scroll.scroll_pin,
            &self.ai_tracker,
            &self.config.terminal,
        );
        self.ensure_animation_running();
        self.request_redraw();
    }

    /// Return the `PaneId` of the pane under the current mouse cursor, if any.
    fn pane_id_at_cursor(&self) -> Option<PaneId> {
        let (x, y) = self.last_cursor_pos?;
        let gpu = self.gpu.as_ref()?;
        let ws_viewport = workspace_viewport(
            &gpu.surface_config,
            self.config.appearance.status_bar_height * self.scale_factor,
        );
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
        if !self.animation.running {
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
            size: size.max(6.0) * self.scale_factor,
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

    fn handle_open_command_palette(&mut self) {
        self.search_overlay.close();
        self.command_palette.open();
        self.refresh_command_palette_items();
        self.request_redraw();
    }

    fn command_palette_entries(&self) -> Vec<CommandPaletteEntry> {
        let mut entries = base_command_palette_entries();

        if let Some(version) = &self.update_available {
            entries.push(CommandPaletteEntry {
                label: format!("Update Scribe to v{version}"),
                action: AutomationAction::OpenUpdateDialog,
            });
        }

        let active_profile = scribe_common::profiles::active_profile_name().ok();
        entries.extend(profile_command_palette_entries(active_profile.as_deref()));
        entries
    }

    fn refresh_command_palette_items(&mut self) {
        let query = self.command_palette.query().trim().to_lowercase();
        let mut items = self.command_palette_entries();
        if !query.is_empty() {
            items.retain(|item| item.label.to_lowercase().contains(&query));
        }
        self.command_palette_items = items;
        self.command_palette.clamp_selection(self.command_palette_items.len());
    }

    fn handle_open_find(&mut self) {
        if self.focused_session_id().is_none() {
            return;
        }
        self.command_palette.close();
        self.command_palette_items.clear();
        self.search_overlay.open();
        self.request_redraw();
    }

    fn handle_command_palette_keyboard(&mut self, event: &winit::event::KeyEvent) -> bool {
        if !self.command_palette.is_active() {
            return false;
        }

        if event.state != winit::event::ElementState::Pressed {
            return true;
        }

        if input::any_matches(&self.bindings.command_palette, event, self.modifiers) {
            return true;
        }

        match &event.logical_key {
            Key::Named(NamedKey::Escape) => {
                self.command_palette.close();
                self.command_palette_items.clear();
                self.request_redraw();
            }
            Key::Named(NamedKey::Enter) => {
                let action = self
                    .command_palette_items
                    .get(self.command_palette.selected_index())
                    .map(|item| item.action.clone());
                self.command_palette.close();
                self.command_palette_items.clear();
                if let Some(action) = action {
                    self.execute_automation_action(action);
                }
                self.request_redraw();
            }
            Key::Named(NamedKey::ArrowDown | NamedKey::Tab) => {
                self.command_palette.next_item(self.command_palette_items.len());
                self.request_redraw();
            }
            Key::Named(NamedKey::ArrowUp) => {
                self.command_palette.prev_item(self.command_palette_items.len());
                self.request_redraw();
            }
            Key::Named(NamedKey::Backspace) => {
                self.command_palette.pop_char();
                self.refresh_command_palette_items();
                self.request_redraw();
            }
            Key::Named(NamedKey::Delete) => {
                self.command_palette.clear_query();
                self.refresh_command_palette_items();
                self.request_redraw();
            }
            Key::Character(text)
                if !self.modifiers.control_key()
                    && !self.modifiers.alt_key()
                    && !self.modifiers.super_key() =>
            {
                let mut changed = false;
                for ch in text.chars().filter(|ch| !ch.is_control()) {
                    self.command_palette.push_char(ch);
                    changed = true;
                }
                if changed {
                    self.refresh_command_palette_items();
                    self.request_redraw();
                }
            }
            _ => {}
        }

        true
    }

    fn handle_search_overlay_keyboard(&mut self, event: &winit::event::KeyEvent) -> bool {
        if !self.search_overlay.is_active() {
            return false;
        }

        if event.state != winit::event::ElementState::Pressed {
            return true;
        }

        if input::any_matches(&self.bindings.find, event, self.modifiers) {
            return true;
        }

        match &event.logical_key {
            Key::Named(NamedKey::Escape) => {
                self.search_overlay.close();
                self.request_redraw();
            }
            Key::Named(NamedKey::Enter) => {
                if self.modifiers.shift_key() {
                    self.search_overlay.prev_match();
                } else {
                    self.search_overlay.next_match();
                }
                self.scroll_focused_pane_to_search_match();
                self.request_redraw();
            }
            Key::Named(NamedKey::ArrowDown) => {
                self.search_overlay.next_match();
                self.scroll_focused_pane_to_search_match();
                self.request_redraw();
            }
            Key::Named(NamedKey::ArrowUp) => {
                self.search_overlay.prev_match();
                self.scroll_focused_pane_to_search_match();
                self.request_redraw();
            }
            Key::Named(NamedKey::Backspace) => {
                self.search_overlay.pop_char();
                self.refresh_search_overlay_results();
            }
            Key::Named(NamedKey::Delete) => {
                self.search_overlay.clear_query();
                self.refresh_search_overlay_results();
            }
            Key::Character(text)
                if !self.modifiers.control_key()
                    && !self.modifiers.alt_key()
                    && !self.modifiers.super_key() =>
            {
                let mut changed = false;
                for ch in text.chars().filter(|ch| !ch.is_control()) {
                    self.search_overlay.push_char(ch);
                    changed = true;
                }
                if changed {
                    self.refresh_search_overlay_results();
                }
            }
            _ => {}
        }

        true
    }

    fn refresh_search_overlay_results(&mut self) {
        let query = self.search_overlay.query().to_owned();
        self.search_overlay.clear_results();

        if query.is_empty() {
            self.request_redraw();
            return;
        }

        let Some(session_id) = self.focused_session_id() else {
            self.request_redraw();
            return;
        };
        let Some(tx) = &self.cmd_tx else {
            self.request_redraw();
            return;
        };

        send_command(
            tx,
            ClientCommand::SearchRequest { session_id, query, limit: SEARCH_RESULT_LIMIT },
        );
        self.request_redraw();
    }

    fn scroll_focused_pane_to_search_match(&mut self) {
        let Some(search_match) = self.search_overlay.current_match().cloned() else { return };
        let Some(tab) = self.window_layout.active_tab() else { return };
        let Some(pane) = self.panes.get_mut(&tab.focused_pane) else { return };

        let current_offset = pane.term.grid().display_offset();
        let current_offset_i32 = saturating_usize_to_i32(current_offset);
        let visible_top = -current_offset_i32;
        let visible_bottom = i32::from(pane.grid.rows.saturating_sub(1)) - current_offset_i32;
        if search_match.row >= visible_top && search_match.row <= visible_bottom {
            return;
        }

        let target_screen_row = i32::from(pane.grid.rows.saturating_sub(1)) / 2;
        let history_size = saturating_usize_to_i32(pane.term.grid().history_size());
        let target_offset_i32 = (target_screen_row - search_match.row).clamp(0, history_size);
        let Ok(target_offset) = usize::try_from(target_offset_i32) else { return };
        let delta = history_size_delta(target_offset, current_offset);
        if delta == 0 {
            return;
        }

        pane.term.scroll_display(alacritty_terminal::grid::Scroll::Delta(delta));
        pane.scrollbar_state.on_scroll_action();
        pane.content_dirty = true;
        self.ensure_animation_running();
        self.request_redraw();
    }

    // -----------------------------------------------------------------------
    // Settings
    // -----------------------------------------------------------------------

    /// Open the settings webview window via the persistent GTK thread.
    ///
    /// Send the current workspace split tree to the server so it can be
    /// persisted for reconnect and handoff.
    fn report_workspace_tree(&mut self) {
        if let Some(tx) = &self.cmd_tx {
            // Invert the session→pane map so serialisation can look up the session
            // for each PaneId in the layout tree.
            let pane_to_session: std::collections::HashMap<PaneId, SessionId> =
                self.session_to_pane.iter().map(|(&sid, &pid)| (pid, sid)).collect();
            let tree = self.window_layout.to_tree(&pane_to_session);
            send_command(tx, ClientCommand::ReportWorkspaceTree { tree });
        }
        self.mark_restore_dirty();
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
        let mode = if sgr {
            mouse_reporting::MouseReportMode::Sgr
        } else {
            mouse_reporting::MouseReportMode::X10
        };
        let data = mouse_reporting::encode_mouse_press(button, col, row, self.modifiers, mode);
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
        let mode = if sgr {
            mouse_reporting::MouseReportMode::Sgr
        } else {
            mouse_reporting::MouseReportMode::X10
        };
        let data = mouse_reporting::encode_mouse_release(button, col, row, self.modifiers, mode);
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
    fn handle_mouse_press(&mut self) {
        let Some((x, y)) = self.last_cursor_pos else { return };
        let Some((ws_viewport, ws_rects)) = self.mouse_press_context() else { return };

        if self.handle_status_bar_mouse_press(x, y)
            || self.handle_tab_bar_mouse_press(x, y)
            || self.handle_prompt_bar_mouse_press(x, y)
            || self.handle_drag_chrome_mouse_press(x, y, ws_viewport, &ws_rects)
        {
            return;
        }

        self.finish_selection_mouse_press(x, y, &ws_rects);
    }

    fn mouse_press_context(&self) -> Option<MousePressContext> {
        let gpu = self.gpu.as_ref()?;
        let ws_viewport = workspace_viewport(
            &gpu.surface_config,
            self.config.appearance.status_bar_height * self.scale_factor,
        );
        Some((ws_viewport, self.window_layout.compute_workspace_rects(ws_viewport)))
    }

    fn handle_status_bar_mouse_press(&mut self, x: f32, y: f32) -> bool {
        if self.status_bar_gear_rect.is_some_and(|rect| rect.contains(x, y)) {
            open_or_focus_settings();
            return true;
        }
        if self.status_bar_equalize_rect.is_some_and(|rect| rect.contains(x, y)) {
            self.window_layout.equalize_all_workspace_ratios();
            self.resize_all_workspace_panes();
            self.report_workspace_tree();
            self.request_redraw();
            return true;
        }
        if self.status_bar_update_rect.is_some_and(|rect| rect.contains(x, y)) {
            self.open_update_dialog();
            return true;
        }
        false
    }

    fn handle_tab_bar_mouse_press(&mut self, x: f32, y: f32) -> bool {
        if let Some((ws_id, _)) =
            self.tab_bar_equalize_targets.iter().find(|(_, rect)| rect.contains(x, y)).copied()
        {
            if let Some(tab) =
                self.window_layout.find_workspace_mut(ws_id).and_then(|ws| ws.active_tab_mut())
            {
                tab.pane_layout.equalize_all_ratios();
            }
            self.resize_after_layout_change();
            self.request_redraw();
            return true;
        }
        if let Some((ws_id, tab_idx)) = self
            .tab_close_hit_targets
            .iter()
            .find_map(|(ws_id, idx, rect)| rect.contains(x, y).then_some((*ws_id, *idx)))
        {
            self.close_tab_by_index(ws_id, tab_idx);
            return true;
        }
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
            return true;
        }
        false
    }

    fn handle_prompt_bar_mouse_press(&mut self, x: f32, y: f32) -> bool {
        let old_prompt_bar_pressed = self.prompt_bar_pressed;
        self.prompt_bar_pressed = self.prompt_bar_target_at(x, y);
        if self.prompt_bar_pressed != old_prompt_bar_pressed {
            self.request_redraw();
        }
        if matches!(self.prompt_bar_pressed, Some((_, prompt_bar::PromptBarHover::DismissButton))) {
            return true;
        }
        if self.try_copy_prompt_bar_text(x, y) {
            return true;
        }
        if self.prompt_bar_pressed.take().is_some() {
            self.request_redraw();
        }
        false
    }

    fn handle_drag_chrome_mouse_press(
        &mut self,
        x: f32,
        y: f32,
        ws_viewport: Rect,
        ws_rects: &[(WorkspaceId, Rect)],
    ) -> bool {
        self.try_scroll_pin_jump(x, y)
            || self.try_start_scrollbar_interaction(x, y)
            || self.try_start_workspace_divider_drag(x, y, ws_viewport)
            || self.try_start_divider_drag(x, y, ws_rects)
            || self.try_start_pane_drag(x, y, ws_rects)
    }

    fn finish_selection_mouse_press(&mut self, x: f32, y: f32, ws_rects: &[(WorkspaceId, Rect)]) {
        self.focus_pane_at(x, y, ws_rects);
        if self.try_open_hovered_url() {
            return;
        }
        if self.try_forward_mouse_press(winit::event::MouseButton::Left) {
            return;
        }
        if !self.modifiers.shift_key() && self.try_prompt_click_to_move() {
            return;
        }
        if self.modifiers.shift_key() && self.active_selection.is_some() {
            self.extend_selection_to(x, y);
            return;
        }
        self.start_selection(x, y);
        match self.mouse_click.record_press(x, y) {
            mouse_state::ClickKind::Single => {}
            mouse_state::ClickKind::Double => self.start_selection_word(x, y),
            mouse_state::ClickKind::Triple => self.start_selection_line(x, y),
        }
    }

    /// Try to move the shell cursor by sending arrow sequences when a left click
    /// lands in the active prompt input zone (OSC 133;B to cursor, same line).
    ///
    /// Returns `true` when arrow sequences were sent and the click is consumed.
    fn try_prompt_click_to_move(&self) -> bool {
        let Some((x, y)) = self.last_cursor_pos else { return false };
        let Some(tab) = self.window_layout.active_tab() else { return false };
        let Some(pane) = self.panes.get(&tab.focused_pane) else { return false };

        if !pane.prompt_ui.click_events || pane.input_start.is_none() {
            return false;
        }
        if pane.term.grid().display_offset() != 0 {
            return false;
        }
        if pane.has_mouse_mode() {
            return false;
        }

        let Some((click_col, click_row)) = self.pixel_to_term_cell(x, y) else { return false };

        let cursor = pane.term.grid().cursor.point;
        let cursor_row = usize::try_from(cursor.line.0.max(0)).unwrap_or(usize::MAX);
        let cursor_col = cursor.column.0;

        if usize::from(click_row) != cursor_row {
            return false;
        }

        let displacement = i32::from(click_col) - saturating_usize_to_i32(cursor_col);
        if displacement == 0 {
            return false;
        }

        let (arrow_seq, count) = if displacement > 0 {
            (b"\x1b[C".as_ref(), usize::try_from(displacement).unwrap_or(usize::MAX))
        } else {
            (b"\x1b[D".as_ref(), usize::try_from(-displacement).unwrap_or(usize::MAX))
        };

        let mut bytes = Vec::with_capacity(count * 3);
        for _ in 0..count {
            bytes.extend_from_slice(arrow_seq);
        }
        self.send_bytes_to_focused_pane(bytes);
        true
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
        let scrollbar_width =
            self.config.appearance.scrollbar_width.clamp(2.0, 20.0) * self.scale_factor;
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
                let delta = history_size_delta(target, display_offset);
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
                update_split_scroll(
                    pane,
                    self.config.terminal.scroll.scroll_pin,
                    &self.ai_tracker,
                    &self.config.terminal,
                );
            }
        }

        self.ensure_animation_running();
        self.request_redraw();
        true
    }

    /// Handle scrollbar drag movement.
    fn handle_scrollbar_drag(&mut self, pane_id: layout::PaneId) {
        let Some((_, y)) = self.last_cursor_pos else { return };
        let scrollbar_width =
            self.config.appearance.scrollbar_width.clamp(2.0, 20.0) * self.scale_factor;
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
            history_size_delta(target_offset, current_offset)
        };

        // Phase 2: mutate.
        if delta != 0 {
            let Some(pane) = self.panes.get_mut(&pane_id) else { return };
            pane.term.scroll_display(alacritty_terminal::grid::Scroll::Delta(delta));
            pane.content_dirty = true;
            update_split_scroll(
                pane,
                self.config.terminal.scroll.scroll_pin,
                &self.ai_tracker,
                &self.config.terminal,
            );
        }
        self.request_redraw();
    }

    /// Update scrollbar hover state for the focused pane.
    fn update_scrollbar_hover(&mut self) {
        let Some((x, y)) = self.last_cursor_pos else { return };
        let tab = self.window_layout.active_tab();
        let Some(tab) = tab else { return };
        let focused_pane_id = tab.focused_pane;
        let scrollbar_width =
            self.config.appearance.scrollbar_width.clamp(2.0, 20.0) * self.scale_factor;
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

            let old_session = self.focused_session_id();

            // Switch workspace focus if needed.
            self.window_layout.set_focused_workspace(*ws_id);

            // Switch pane focus within the workspace.
            if let Some(active) = self.window_layout.active_tab_mut() {
                active.focused_pane = *clicked_pane;
            }

            let new_session = self.focused_session_id();
            if old_session != new_session {
                self.notify_focus_change(new_session, old_session);
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

        // Ensure animation is ticking during selection drag so
        // edge-scroll fires at a steady rate via the animation tick.
        if self.mouse_selecting {
            self.ensure_animation_running();
        }

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

        // Pane drag update.
        if self.pane_drag.is_some() {
            self.update_pane_drag(x, y);
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

        // Update tab hover state (for background highlight on inactive tabs).
        let new_tab_hover = self
            .tab_hit_targets
            .iter()
            .find_map(|(ws_id, idx, rect)| rect.contains(x, y).then_some((*ws_id, *idx)));
        if new_tab_hover != self.hovered_tab {
            self.hovered_tab = new_tab_hover;
            self.request_redraw();
        }

        // Forward motion events to PTY when mouse motion reporting is active.
        self.maybe_forward_mouse_motion(x, y);

        let hovered_url_changed = !self.mouse_selecting && self.refresh_hovered_url();

        // No drag active — update cursor icon based on divider hover.
        self.update_hover_cursor(x, y);

        // Request a redraw if the active link highlight changed.
        if hovered_url_changed {
            self.request_redraw();
        }

        // Prompt bar hover detection.
        self.update_prompt_bar_hover(x, y);

        // Split-scroll jump button hover detection.
        self.update_scroll_pin_hover(x, y);

        // Update tooltip state: check status bar targets (Above) then tab targets (Below).
        self.update_active_tooltip(x, y);
    }

    /// Check tooltip hover targets and update `active_tooltip`.
    ///
    /// Status bar targets show Above; tab bar targets show Below.
    fn update_active_tooltip(&mut self, cursor_x: f32, cursor_y: f32) {
        let new_tooltip = self
            .status_bar_tooltip_targets
            .iter()
            .find(|t| t.rect.contains(cursor_x, cursor_y))
            .map(|t| (t.text.clone(), t.rect, tooltip::TooltipPosition::Above))
            .or_else(|| {
                self.tab_bar_tooltip_targets
                    .iter()
                    .find(|t| t.rect.contains(cursor_x, cursor_y))
                    .map(|t| (t.text.clone(), t.rect, tooltip::TooltipPosition::Below))
            });

        let changed = match (&self.active_tooltip, &new_tooltip) {
            (None, None) => false,
            (Some((prev_text, _, _)), Some((new_text, _, _))) => prev_text != new_text,
            _ => true,
        };
        if changed {
            self.active_tooltip = new_tooltip;
            self.request_redraw();
        }
    }

    /// Update prompt bar hover state by hit-testing the cursor against
    /// prompt bars in all visible panes.
    fn update_prompt_bar_hover(&mut self, mouse_x: f32, mouse_y: f32) {
        let old_hover = self.prompt_bar_hover;
        self.prompt_bar_hover = self.prompt_bar_target_at(mouse_x, mouse_y);

        if self.prompt_bar_hover != old_hover {
            self.request_redraw();
        }
    }

    /// Update scroll-pin jump button hover state.
    fn update_scroll_pin_hover(&mut self, mouse_x: f32, mouse_y: f32) {
        let old = self.scroll_pin_hover;
        self.scroll_pin_hover = None;

        if !self.config.terminal.scroll.scroll_pin {
            if old.is_some() {
                self.request_redraw();
            }
            return;
        }

        let Some(gpu) = &self.gpu else {
            if old.is_some() {
                self.request_redraw();
            }
            return;
        };
        let cell_size = gpu.renderer.cell_size();
        let cell_h = cell_size.height;

        let pb_font_scale =
            self.config.terminal.prompt_bar.font_size / self.config.appearance.font_size;
        let pb_cell_h = cell_h * pb_font_scale;
        let pb_at_top = self.config.terminal.prompt_bar.position
            == scribe_common::config::PromptBarPosition::Top;

        for (pane_id, pane_rect, pane_edges) in self.visible_pane_rects() {
            let Some(pane) = self.panes.get(&pane_id) else { continue };
            let Some(ss) = &pane.split_scroll else { continue };
            if ss.pin_height <= 0.0 {
                continue;
            }
            let tbh = if pane_edges.top() {
                self.tab_bar_height_for(pane.workspace_id, pane_rect)
            } else {
                0.0
            };
            let pbh = pane.prompt_bar_height(pb_cell_h, self.config.terminal.prompt_bar.enabled);
            let content_pbh = if pb_at_top { pbh } else { 0.0 };
            let content_offset = pane.content_offset(
                tbh,
                content_pbh,
                &self.config.appearance.content_padding,
                self.scale_factor,
            );
            let screen_lines = pane.term.grid().screen_lines();
            let content_h = usize_to_f32(screen_lines) * cell_h;
            let geo = split_scroll::compute_geometry(
                layout::Rect {
                    x: content_offset.0,
                    y: content_offset.1,
                    width: f32::from(pane.grid.cols) * cell_size.width,
                    height: content_h,
                },
                ss.pin_height,
            );
            if split_scroll::hit_test_jump_btn(&geo, mouse_x, mouse_y) {
                self.scroll_pin_hover = Some(pane_id);
                break;
            }
        }

        if self.scroll_pin_hover != old {
            self.request_redraw();
        }
    }

    /// Jump to bottom if the click lands on the split-scroll jump button.
    fn try_scroll_pin_jump(&mut self, x: f32, y: f32) -> bool {
        if !self.config.terminal.scroll.scroll_pin {
            return false;
        }
        let Some(gpu) = &self.gpu else { return false };
        let cell_size = gpu.renderer.cell_size();
        let cell_h = cell_size.height;

        let pb_font_scale =
            self.config.terminal.prompt_bar.font_size / self.config.appearance.font_size;
        let pb_cell_h = cell_h * pb_font_scale;
        let pb_at_top = self.config.terminal.prompt_bar.position
            == scribe_common::config::PromptBarPosition::Top;

        let target =
            self.visible_pane_rects().into_iter().find_map(|(pane_id, pane_rect, pane_edges)| {
                let pane = self.panes.get(&pane_id)?;
                let ss = pane.split_scroll.as_ref()?;
                if ss.pin_height <= 0.0 {
                    return None;
                }
                let tbh = if pane_edges.top() {
                    self.tab_bar_height_for(pane.workspace_id, pane_rect)
                } else {
                    0.0
                };
                let pbh =
                    pane.prompt_bar_height(pb_cell_h, self.config.terminal.prompt_bar.enabled);
                let content_pbh = if pb_at_top { pbh } else { 0.0 };
                let content_offset = pane.content_offset(
                    tbh,
                    content_pbh,
                    &self.config.appearance.content_padding,
                    self.scale_factor,
                );
                let screen_lines = pane.term.grid().screen_lines();
                let content_h = usize_to_f32(screen_lines) * cell_h;
                let geo = split_scroll::compute_geometry(
                    layout::Rect {
                        x: content_offset.0,
                        y: content_offset.1,
                        width: f32::from(pane.grid.cols) * cell_size.width,
                        height: content_h,
                    },
                    ss.pin_height,
                );
                split_scroll::hit_test_jump_btn(&geo, x, y).then_some(pane_id)
            });
        let Some(pane_id) = target else { return false };
        let Some(pane) = self.panes.get_mut(&pane_id) else { return false };
        pane.split_scroll = None;
        pane.term.scroll_display(alacritty_terminal::grid::Scroll::Bottom);
        pane.scrollbar_state.on_scroll_action();
        pane.content_dirty = true;
        self.scroll_pin_hover = None;
        self.ensure_animation_running();
        self.request_redraw();
        true
    }

    /// Dismiss the prompt bar if the click lands on the hover-only × overlay.
    ///
    /// Returns `true` when the click was consumed.
    fn try_dismiss_prompt_bar(&mut self, x: f32, y: f32) -> bool {
        let Some((pane_id, hover)) = self.prompt_bar_target_at(x, y) else { return false };
        if hover != prompt_bar::PromptBarHover::DismissButton {
            return false;
        }
        if let Some(pane) = self.panes.get_mut(&pane_id) {
            pane.prompt_ui.dismissed = true;
        }
        self.prompt_bar_hover = None;
        self.prompt_bar_pressed = None;
        self.resize_after_prompt_bar_height_change();
        self.request_redraw();
        true
    }

    /// Copy the full prompt text to clipboard when a prompt line is clicked.
    ///
    /// Returns `true` when the click was consumed.
    fn try_copy_prompt_bar_text(&mut self, x: f32, y: f32) -> bool {
        let text = self.prompt_bar_target_at(x, y).and_then(|(pane_id, hover)| {
            let pane = self.panes.get(&pane_id)?;
            prompt_bar::hovered_prompt_text(pane, hover).map(str::to_owned)
        });

        let Some(text) = text else { return false };
        if let Some(cb) = self.clipboard.as_mut() {
            if let Err(e) = cb.set_text(text) {
                tracing::warn!("clipboard write failed: {e}");
            }
        }
        true
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
        let mode = if sgr {
            mouse_reporting::MouseReportMode::Sgr
        } else {
            mouse_reporting::MouseReportMode::X10
        };
        let data =
            mouse_reporting::encode_mouse_motion(col, row, button_held, self.modifiers, mode);
        self.send_bytes_to_focused_pane(data);
    }

    /// Set the window cursor icon based on whether the pointer is hovering over
    /// a divider. Resets to the default arrow cursor when not over any divider.
    fn update_hover_cursor(&self, x: f32, y: f32) {
        let Some(gpu) = &self.gpu else { return };
        let Some(window) = &self.window else { return };
        let ws_viewport = workspace_viewport(
            &gpu.surface_config,
            self.config.appearance.status_bar_height * self.scale_factor,
        );

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
            if terminal_content_hit(tab, *ws_rect, x, y, tab_bar_h) {
                return true;
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

    /// If Ctrl is held and a URL or path is hovered, open it.
    ///
    /// Returns `true` if a span was opened.
    pub fn try_open_hovered_url(&mut self) -> bool {
        if !self.modifiers.control_key() {
            return false;
        }
        if let Some(ref span) = self.hovered_url {
            let kind_str = match span.kind {
                url_detect::SpanKind::Url => "url",
                url_detect::SpanKind::Path => "path",
            };
            tracing::debug!(url = %span.url, kind = kind_str, "ctrl+click: opening hovered span");
            let text = span.url.clone();
            match span.kind {
                url_detect::SpanKind::Url => url_detect::open_url(&text),
                url_detect::SpanKind::Path => {
                    let cwd = self
                        .window_layout
                        .active_tab()
                        .and_then(|t| self.panes.get(&t.focused_pane))
                        .and_then(|p| p.cwd.as_deref());
                    url_detect::open_path(&text, cwd);
                }
            }
            return true;
        }
        tracing::debug!(
            hovered = self.hovered_url.is_some(),
            ctrl = self.modifiers.control_key(),
            "ctrl+click: no hovered url to open"
        );
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
        self.line_drag_anchor = None;
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
        self.line_drag_anchor = Some((start, end));
        self.mouse_selecting = true;
        self.request_redraw();
    }

    /// Extend the active selection to the given pixel position (shift+click).
    fn extend_selection_to(&mut self, x: f32, y: f32) {
        let Some(sel) = self.active_selection else { return };
        let Some(point) = self.cursor_to_grid(x, y) else { return };
        match sel.mode {
            mouse_state::SelectionMode::Cell => {
                self.active_selection = Some(selection::SelectionRange::cell(sel.start, point));
                self.request_redraw();
            }
            mouse_state::SelectionMode::Word => {
                self.extend_selection_word_with_fallback(sel, point);
            }
            mouse_state::SelectionMode::Line => {
                self.extend_selection_line_with_fallback(sel, point);
            }
        }
    }

    fn extend_selection_word_with_fallback(
        &mut self,
        sel: selection::SelectionRange,
        point: selection::SelectionPoint,
    ) {
        let (anchor_start, anchor_end) = self.word_drag_anchor.unwrap_or((sel.start, sel.end));
        let Some(tab) = self.window_layout.active_tab() else { return };
        let pane_id = tab.focused_pane;
        let Some(pane) = self.panes.get(&pane_id) else { return };
        let new_sel = selection::extend_by_word(&pane.term, anchor_start, anchor_end, point);
        self.active_selection = Some(new_sel);
        self.request_redraw();
    }

    fn extend_selection_line_with_fallback(
        &mut self,
        sel: selection::SelectionRange,
        point: selection::SelectionPoint,
    ) {
        let (anchor_start, anchor_end) = self.line_drag_anchor.unwrap_or((sel.start, sel.end));
        let Some(tab) = self.window_layout.active_tab() else { return };
        let pane_id = tab.focused_pane;
        let Some(pane) = self.panes.get(&pane_id) else { return };
        let new_sel = selection::extend_by_line(&pane.term, anchor_start, anchor_end, point);
        self.active_selection = Some(new_sel);
        self.request_redraw();
    }

    /// Extend the in-progress selection to the current cursor position.
    fn extend_selection(&mut self) {
        if !self.mouse_selecting {
            return;
        }
        let Some(sel) = self.active_selection else { return };
        let Some((x, y)) = self.last_cursor_pos else { return };
        let Some(point) = self.cursor_to_grid_impl(x, y, true) else { return };
        match sel.mode {
            mouse_state::SelectionMode::Cell => {
                self.active_selection = Some(selection::SelectionRange::cell(sel.start, point));
                self.request_redraw();
            }
            mouse_state::SelectionMode::Word => {
                self.extend_selection_word(point);
            }
            mouse_state::SelectionMode::Line => {
                self.extend_selection_line(point);
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

    fn extend_selection_line(&mut self, point: selection::SelectionPoint) {
        let Some((anchor_start, anchor_end)) = self.line_drag_anchor else { return };
        let pane_id = {
            let Some(tab) = self.window_layout.active_tab() else { return };
            tab.focused_pane
        };
        let Some(pane) = self.panes.get(&pane_id) else { return };
        let new_sel = selection::extend_by_line(&pane.term, anchor_start, anchor_end, point);
        self.active_selection = Some(new_sel);
        self.request_redraw();
    }

    /// Finalize selection on mouse release and auto-copy if enabled.
    fn handle_mouse_release(&mut self) {
        let pressed_prompt_bar = self.prompt_bar_pressed;
        let released_prompt_bar =
            self.last_cursor_pos.and_then(|(x, y)| self.prompt_bar_target_at(x, y));
        if self.prompt_bar_pressed.take().is_some() {
            self.request_redraw();
        }
        if let (Some((x, y)), Some((_, prompt_bar::PromptBarHover::DismissButton))) =
            (self.last_cursor_pos, pressed_prompt_bar)
        {
            if released_prompt_bar == pressed_prompt_bar && self.try_dismiss_prompt_bar(x, y) {
                return;
            }
        }
        self.mouse_selecting = false;
        self.finish_tab_drag();
        self.finish_pane_drag();
        if !self.config.terminal.clipboard.copy_on_select {
            return;
        }
        self.finalize_copy();
        #[cfg(target_os = "linux")]
        self.set_primary_selection();
    }

    /// Scroll the focused pane if the cursor is near the top/bottom edge during drag selection.
    /// Returns `true` if scrolling happened.
    fn maybe_edge_scroll(&mut self) -> bool {
        if !self.mouse_selecting {
            return false;
        }
        let Some((_, cursor_y)) = self.last_cursor_pos else { return false };
        let tab_bar_h = self.focused_workspace_tab_bar_height();
        let Some(pane_rect) = self.focused_pane_rect() else { return false };
        let pb_at_top = self.config.terminal.prompt_bar.position
            == scribe_common::config::PromptBarPosition::Top;
        let pbh = self.gpu.as_ref().map_or(0.0, |gpu| {
            let cell = gpu.renderer.cell_size();
            let pb_font_scale =
                self.config.terminal.prompt_bar.font_size / self.config.appearance.font_size;
            let pb_cell_h = cell.height * pb_font_scale;
            let tab = self.window_layout.active_tab();
            tab.and_then(|t| self.panes.get(&t.focused_pane)).map_or(0.0, |p| {
                p.prompt_bar_height(pb_cell_h, self.config.terminal.prompt_bar.enabled)
            })
        });
        let content_top = pane_rect.y + tab_bar_h + if pb_at_top { pbh } else { 0.0 };
        let content_bottom = pane_rect.y + pane_rect.height - if pb_at_top { 0.0 } else { pbh };
        let Some(delta) = mouse_state::edge_scroll_delta(cursor_y, content_top, content_bottom)
        else {
            return false;
        };
        let pane_id = self.window_layout.active_tab().map(|t| t.focused_pane);
        let Some(pane_id) = pane_id else { return false };
        let Some(pane) = self.panes.get_mut(&pane_id) else { return false };
        pane.term.scroll_display(alacritty_terminal::grid::Scroll::Delta(delta));
        pane.scrollbar_state.on_scroll_action();
        pane.content_dirty = true;
        self.extend_selection();
        self.ensure_animation_running();
        self.request_redraw();
        true
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
        self.flush_resize_if_pending();
        let Some(tx) = self.cmd_tx.clone() else { return };
        let focused_pane_id = {
            let Some(tab) = self.window_layout.active_tab() else { return };
            tab.focused_pane
        };
        let scrolled_up = {
            let Some(pane) = self.panes.get_mut(&focused_pane_id) else { return };
            let offset = pane.term.grid().display_offset();
            if offset > 0 {
                pane.split_scroll = None;
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
        if tx
            .send(ClientCommand::KeyInput {
                session_id: pane.session_id,
                data,
                dismisses_attention: false,
            })
            .is_err()
        {
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
        let (raw, cleanup_active) = {
            let Some(tab) = self.window_layout.active_tab() else { return };
            let Some(pane) = self.panes.get(&tab.focused_pane) else { return };
            let text = selection::extract_text(&pane.term, &sel);
            let cleanup_active = self.ai_tracker.provider_for_session(pane.session_id).is_some();
            (text, cleanup_active)
        };
        if raw.is_empty() {
            return;
        }
        let text = clipboard_cleanup::prepare_copy_text(
            &raw,
            clipboard_cleanup::CopyTextOptions {
                ai_session_active: cleanup_active,
                cleanup_enabled: self.config.terminal.clipboard.claude_copy_cleanup,
            },
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
        let (url, file_path) =
            self.hovered_url.as_ref().map_or((None, None), |span| match span.kind {
                url_detect::SpanKind::Url => (Some(span.url.clone()), None),
                url_detect::SpanKind::Path => (None, Some(span.url.clone())),
            });
        self.context_menu =
            Some(context_menu::ContextMenu::new(x, y, has_selection, url, file_path));
        self.request_redraw();
    }

    /// Dispatch an action selected from the context menu.
    fn dispatch_context_menu_action(&mut self, action: context_menu::ContextMenuAction) {
        match action {
            context_menu::ContextMenuAction::Copy => self.finalize_copy(),
            context_menu::ContextMenuAction::Paste => self.perform_paste(),
            context_menu::ContextMenuAction::SelectAll => self.select_all(),
            context_menu::ContextMenuAction::OpenUrl(url) => url_detect::open_url(&url),
            context_menu::ContextMenuAction::OpenFile(path) => {
                let cwd = self
                    .window_layout
                    .active_tab()
                    .and_then(|t| self.panes.get(&t.focused_pane))
                    .and_then(|p| p.cwd.as_deref());
                url_detect::open_path(&path, cwd);
            }
        }
        self.request_redraw();
    }

    /// Select all content in the focused pane (viewport + scrollback).
    fn select_all(&mut self) {
        use alacritty_terminal::grid::Dimensions as _;
        let Some(tab) = self.window_layout.active_tab() else { return };
        let Some(pane) = self.panes.get(&tab.focused_pane) else { return };
        let history = saturating_usize_to_i32(pane.term.grid().history_size());
        let last_row = saturating_usize_to_i32(pane.term.grid().screen_lines().saturating_sub(1));
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
            self.report_workspace_tree();
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

    /// Compute the hit rect for a pane's title pill given the pane rect and title.
    ///
    /// Mirrors the positioning logic in `tab_bar::build_pane_title_pill`.
    /// Returns `None` when the pill would be invisible (too narrow or zero cell size).
    fn pane_pill_rect(
        &self,
        pane_rect: layout::Rect,
        title: &str,
        tab_bar_height: f32,
    ) -> Option<layout::Rect> {
        let gpu = self.gpu.as_ref()?;
        let cell = gpu.renderer.cell_size();
        let cell_w = cell.width;
        let cell_h = cell.height;
        if cell_w <= 0.0 || cell_h <= 0.0 {
            return None;
        }
        let pill_h = tab_bar_height.max(cell_h);
        let pane_cols = main_units_in_extent(pane_rect.width, cell_w);
        let max_content_cols = main_units_in_extent(
            main_grid_pixels(pane_cols, cell_w) * PANE_PILL_MAX_FRACTION,
            cell_w,
        )
        .saturating_sub(2);
        if max_content_cols == 0 {
            return None;
        }
        let content_len = title.chars().count().min(max_content_cols);
        let pill_cols = content_len + 2;
        let pill_width = main_grid_pixels(pill_cols, cell_w);
        let pill_x = (pane_rect.x + pane_rect.width - pill_width - cell_w).max(pane_rect.x);
        let pill_y = pane_rect.y;
        Some(layout::Rect { x: pill_x, y: pill_y, width: pill_width, height: pill_h })
    }

    /// Try to start a pane drag from a title pill click.
    ///
    /// Returns `true` if a pill was hit and the drag was initiated.
    fn try_start_pane_drag(&mut self, x: f32, y: f32, ws_rects: &[(WorkspaceId, Rect)]) -> bool {
        let hit = self.find_pane_pill_at(x, y, ws_rects);
        if let Some((ws_id, pane_id)) = hit {
            self.pane_drag = Some(PaneDrag {
                workspace_id: ws_id,
                pane_id,
                start_x: x,
                start_y: y,
                dragging: false,
            });
            return true;
        }
        false
    }

    /// Find the `(workspace_id, pane_id)` of a pane title pill at pixel `(x, y)`.
    ///
    /// Returns `None` when no pill is hit or when there is only one pane in a workspace.
    fn find_pane_pill_at(
        &self,
        x: f32,
        y: f32,
        ws_rects: &[(WorkspaceId, Rect)],
    ) -> Option<(WorkspaceId, PaneId)> {
        for (ws_id, ws_rect) in ws_rects {
            let Some(tab) =
                self.window_layout.find_workspace(*ws_id).and_then(|ws| ws.active_tab())
            else {
                continue;
            };
            let pane_rects = tab.pane_layout.compute_rects(*ws_rect);
            if pane_rects.len() < 2 {
                continue;
            }
            let tab_bar_h = self.tab_bar_height_for(*ws_id, *ws_rect);
            let hit = pane_rects.iter().find_map(|&(pane_id, pane_rect, _)| {
                let title = self.panes.get(&pane_id).map_or("", |p| p.title.as_str());
                let pill_hit = self
                    .pane_pill_rect(pane_rect, title, tab_bar_h)
                    .is_some_and(|r| r.contains(x, y));
                pill_hit.then_some((*ws_id, pane_id))
            });
            if hit.is_some() {
                return hit;
            }
        }
        None
    }

    /// Update the in-progress pane drag position and threshold.
    fn update_pane_drag(&mut self, x: f32, y: f32) {
        let (was_dragging, now_dragging) = {
            let Some(drag) = self.pane_drag.as_mut() else { return };
            let was = drag.dragging;
            if !drag.dragging {
                let dx = x - drag.start_x;
                let dy = y - drag.start_y;
                drag.dragging = dx * dx + dy * dy > 25.0;
            }
            (was, drag.dragging)
        };
        if !was_dragging && now_dragging {
            if let Some(window) = &self.window {
                window.set_cursor(winit::window::CursorIcon::Grabbing);
            }
        }
    }

    /// Complete the pane drag on mouse release: swap panes if dropped on another pane.
    fn finish_pane_drag(&mut self) {
        let Some(drag) = self.pane_drag.take() else { return };
        if let Some(window) = &self.window {
            window.set_cursor(winit::window::CursorIcon::Default);
        }
        if !drag.dragging {
            return;
        }
        let Some((x, y)) = self.last_cursor_pos else { return };
        let Some(gpu) = &self.gpu else { return };
        let ws_viewport = workspace_viewport(
            &gpu.surface_config,
            self.config.appearance.status_bar_height * self.scale_factor,
        );
        let ws_rects = self.window_layout.compute_workspace_rects(ws_viewport);

        // Find which pane the cursor is over in the same workspace.
        let target_pane = ws_rects.iter().find_map(|(ws_id, ws_rect)| {
            if *ws_id != drag.workspace_id {
                return None;
            }
            let tab = self.window_layout.find_workspace(*ws_id)?.active_tab()?;
            let pane_rects = tab.pane_layout.compute_rects(*ws_rect);
            pane_rects
                .into_iter()
                .find(|(pid, rect, _)| *pid != drag.pane_id && rect.contains(x, y))
                .map(|(pid, _, _)| pid)
        });

        let Some(target_pane) = target_pane else { return };

        // Swap panes in the layout tree.
        let swapped = self
            .window_layout
            .find_workspace_mut(drag.workspace_id)
            .and_then(|ws| ws.active_tab_mut())
            .is_some_and(|tab| tab.pane_layout.swap_panes(drag.pane_id, target_pane));

        if swapped {
            self.resize_after_layout_change();
            self.report_workspace_tree();
            self.request_redraw();
        }
    }

    /// Convert a pixel position to an absolute grid cell in the focused pane.
    ///
    /// The returned row is an absolute grid line (negative = scrollback),
    /// incorporating the pane's current `display_offset`.
    fn cursor_to_grid(&self, x: f32, y: f32) -> Option<selection::SelectionPoint> {
        self.cursor_to_grid_impl(x, y, false)
    }

    /// Convert a pixel position to an absolute grid cell, optionally clamping
    /// points outside the content area to the nearest visible cell.
    fn cursor_to_grid_impl(
        &self,
        x: f32,
        y: f32,
        clamp_to_content: bool,
    ) -> Option<selection::SelectionPoint> {
        let gpu = self.gpu.as_ref()?;
        let cell = gpu.renderer.cell_size();
        let pane_rect = self.focused_pane_rect()?;
        let tab_bar_h = self.focused_workspace_tab_bar_height();
        let tab = self.window_layout.active_tab()?;
        let pane = self.panes.get(&tab.focused_pane)?;
        let display_offset = pane.term.grid().display_offset();
        let pb_font_scale =
            self.config.terminal.prompt_bar.font_size / self.config.appearance.font_size;
        let pb_cell_h = cell.height * pb_font_scale;
        let pbh = pane.prompt_bar_height(pb_cell_h, self.config.terminal.prompt_bar.enabled);
        let pb_at_top = self.config.terminal.prompt_bar.position
            == scribe_common::config::PromptBarPosition::Top;
        let padding = pane::effective_padding(
            &self.config.appearance.content_padding,
            pane.edges,
            self.scale_factor,
        );
        let request = selection::PixelToGridRequest {
            x,
            y,
            pane_rect,
            cell_size: (cell.width, cell.height),
            tab_bar_height: tab_bar_h,
            prompt_bar_height: pbh,
            prompt_bar_at_top: pb_at_top,
            display_offset,
            padding: &padding,
        };
        if clamp_to_content {
            selection::pixel_to_grid_clamped(request)
        } else {
            selection::pixel_to_grid(request)
        }
    }

    /// Compute the screen rect of the currently focused pane.
    fn focused_pane_rect(&self) -> Option<Rect> {
        let gpu = self.gpu.as_ref()?;
        let ws_viewport = workspace_viewport(
            &gpu.surface_config,
            self.config.appearance.status_bar_height * self.scale_factor,
        );
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
        let direction = if lines > 0 {
            mouse_reporting::ScrollDirection::Up
        } else {
            mouse_reporting::ScrollDirection::Down
        };
        let mode = if sgr_mode {
            mouse_reporting::MouseReportMode::Sgr
        } else {
            mouse_reporting::MouseReportMode::X10
        };
        let data = mouse_reporting::encode_mouse_scroll(direction, col, row, self.modifiers, mode);
        self.send_bytes_to_focused_pane(data);
    }

    /// Convert pixel `(x, y)` to a 0-indexed `(col, row)` within the focused
    /// pane's terminal viewport.
    ///
    /// Returns `None` when no GPU context is available, the cursor is outside
    /// the content area, or cell dimensions are zero.
    fn pixel_to_term_cell(&self, x: f32, y: f32) -> Option<(u16, u16)> {
        let gpu = self.gpu.as_ref()?;
        let cell = gpu.renderer.cell_size();
        if cell.width <= 0.0 || cell.height <= 0.0 {
            return None;
        }
        let tab_bar_h = self.focused_workspace_tab_bar_height();
        let tab = self.window_layout.active_tab()?;
        let pane = self.panes.get(&tab.focused_pane)?;
        let pb_font_scale =
            self.config.terminal.prompt_bar.font_size / self.config.appearance.font_size;
        let pb_cell_h = cell.height * pb_font_scale;
        let pb_at_top = self.config.terminal.prompt_bar.position
            == scribe_common::config::PromptBarPosition::Top;
        let pbh = pane.prompt_bar_height(pb_cell_h, self.config.terminal.prompt_bar.enabled);
        let content_pbh = if pb_at_top { pbh } else { 0.0 };
        let (content_x, content_y) = pane.content_offset(
            tab_bar_h,
            content_pbh,
            &self.config.appearance.content_padding,
            self.scale_factor,
        );
        let rel_x = x - content_x;
        let rel_y = y - content_y;
        if rel_x < 0.0 || rel_y < 0.0 {
            return None;
        }
        let col = u16::try_from(main_units_in_extent(rel_x, cell.width))
            .unwrap_or(u16::MAX)
            .min(pane.grid.cols.saturating_sub(1));
        let row = u16::try_from(main_units_in_extent(rel_y, cell.height))
            .unwrap_or(u16::MAX)
            .min(pane.grid.rows.saturating_sub(1));
        Some((col, row))
    }

    /// Send raw bytes to the focused pane's PTY session.
    fn send_bytes_to_focused_pane(&self, data: Vec<u8>) {
        let Some(tx) = self.cmd_tx.clone() else { return };
        let Some(tab) = self.window_layout.active_tab() else { return };
        let Some(pane) = self.panes.get(&tab.focused_pane) else { return };
        send_command(
            &tx,
            ClientCommand::KeyInput {
                session_id: pane.session_id,
                data,
                dismisses_attention: false,
            },
        );
    }

    fn handle_dropped_path(&mut self, path: &std::path::Path) {
        let Some(tab) = self.window_layout.active_tab() else { return };
        let Some(pane) = self.panes.get(&tab.focused_pane) else { return };
        let quoted = quote_path_for_shell(path, &pane.shell_name);
        self.send_paste_data(&format!("{quoted} "));
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
    fn resize_all_panes_from_rects(
        &mut self,
        rects: &[(PaneId, Rect, PaneEdges)],
        ws_rects: &[(WorkspaceId, Rect)],
    ) {
        let Some(gpu) = &self.gpu else { return };
        let cell = gpu.renderer.cell_size();
        tracing::debug!(
            pane_count = rects.len(),
            cell_w = cell.width,
            cell_h = cell.height,
            "resize_all_panes_from_rects"
        );

        // Build per-workspace tab bar heights so each pane uses the correct height.
        let ws_heights: std::collections::HashMap<WorkspaceId, f32> = ws_rects
            .iter()
            .map(|(ws_id, ws_rect)| (*ws_id, self.tab_bar_height_for(*ws_id, *ws_rect)))
            .collect();
        let fallback_h = self.effective_tab_bar_height();

        // Collect (pane_id, history_delta, topmost) so we can adjust selections
        // after the loop without conflicting borrows on self.panes.
        let mut resize_deltas: Vec<(PaneId, i32, i32)> = Vec::new();

        for (pane_id, rect, edges) in rects {
            let Some(pane) = self.panes.get_mut(pane_id) else { continue };
            pane.edges = *edges;
            let tab_bar_h = ws_heights.get(&pane.workspace_id).copied().unwrap_or(fallback_h);
            let tbh = if edges.top() { tab_bar_h } else { 0.0 };
            let eff_pad = pane::effective_padding(
                &self.config.appearance.content_padding,
                *edges,
                self.scale_factor,
            );
            let pb_font_scale =
                self.config.terminal.prompt_bar.font_size / self.config.appearance.font_size;
            let pb_cell_h = cell.height * pb_font_scale;
            let pbh = pane.prompt_bar_height(pb_cell_h, self.config.terminal.prompt_bar.enabled);
            let grid = pane::compute_pane_grid(&pane::PaneGridRequest {
                rect: *rect,
                cell_size: (cell.width, cell.height),
                tab_bar_height: tbh,
                prompt_bar_height: pbh,
                padding: &eff_pad,
            });
            let old_history = pane.term.grid().history_size();
            pane.resize(*rect, grid);
            let new_history = pane.term.grid().history_size();
            let delta = history_size_delta(new_history, old_history);
            let topmost = pane.term.grid().topmost_line().0;
            resize_deltas.push((*pane_id, delta, topmost));
        }

        // Apply selection adjustments for any pane whose scrollback changed.
        for (pane_id, delta, topmost) in resize_deltas {
            if delta == 0 {
                continue;
            }
            let focused_pane = self.window_layout.active_tab().map(|t| t.focused_pane);
            if focused_pane == Some(pane_id) {
                self.shift_active_selection(delta, topmost);
            } else {
                self.shift_background_tab_selection(pane_id, delta, topmost);
            }
        }

        self.resize_pending = Some(Instant::now());
    }

    /// Recompute rects and resize all panes after a layout change.
    fn resize_after_layout_change(&mut self) {
        let Some(gpu) = &self.gpu else { return };
        let ws_viewport = workspace_viewport(
            &gpu.surface_config,
            self.config.appearance.status_bar_height * self.scale_factor,
        );
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

    /// Resize after prompt-bar visibility changes and notify the PTY without
    /// the normal resize debounce.
    ///
    /// Codex repaints as a full-screen synchronized TUI. If the client shrinks
    /// its local grid for the prompt bar but the PTY keeps the old winsize for
    /// the debounce window, old-size Codex frames can be replayed into the
    /// smaller client grid and appear cut off.
    fn resize_after_prompt_bar_height_change(&mut self) {
        self.resize_after_layout_change();
        self.flush_resize_now();
    }

    /// Recompute rects and resize panes in all workspaces.
    ///
    /// Used after workspace splits where the window is re-divided and every
    /// workspace region changes size. Handles both active-tab panes (which
    /// have a full pane layout) and non-active-tab panes (resized to the
    /// full workspace rect).
    fn resize_all_workspace_panes(&mut self) {
        let Some(gpu) = &self.gpu else { return };
        let ws_viewport = workspace_viewport(
            &gpu.surface_config,
            self.config.appearance.status_bar_height * self.scale_factor,
        );
        let cell = gpu.renderer.cell_size();
        let tab_bar_h = self.effective_tab_bar_height();
        let padding = pane::effective_padding(
            &self.config.appearance.content_padding,
            PaneEdges::all_external(),
            self.scale_factor,
        );

        let ws_rects = self.window_layout.compute_workspace_rects(ws_viewport);

        // Pre-compute per-workspace rect→grid for non-active-tab pane resizing.
        let ws_grid_map: HashMap<WorkspaceId, (Rect, GridSize)> = ws_rects
            .iter()
            .map(|&(ws_id, ws_rect)| {
                let grid = pane::compute_pane_grid(&pane::PaneGridRequest {
                    rect: ws_rect,
                    cell_size: (cell.width, cell.height),
                    tab_bar_height: tab_bar_h,
                    prompt_bar_height: 0.0,
                    padding: &padding,
                });
                (ws_id, (ws_rect, grid))
            })
            .collect();

        let all_pane_rects: Vec<_> = ws_rects
            .iter()
            .filter_map(|(ws_id, ws_rect)| {
                let tab = self.window_layout.find_workspace(*ws_id)?.active_tab()?;
                Some(tab.pane_layout.compute_rects(*ws_rect))
            })
            .flatten()
            .collect();

        // Collect pane IDs that are handled by the active-tab resize.
        let active_pane_ids: HashSet<PaneId> =
            all_pane_rects.iter().map(|(pane_id, _, _)| *pane_id).collect();

        self.resize_all_panes_from_rects(&all_pane_rects, &ws_rects);

        // Resize non-active-tab panes to their workspace rect.
        for (pane_id, pane) in &mut self.panes {
            if active_pane_ids.contains(pane_id) {
                continue;
            }
            let Some(&(ws_rect, ws_grid)) = ws_grid_map.get(&pane.workspace_id) else {
                tracing::warn!(
                    pane_id = pane_id.raw(),
                    workspace_id = %pane.workspace_id,
                    "non-active-tab pane has no workspace rect — skipping resize"
                );
                continue;
            };
            pane.resize(ws_rect, ws_grid);
        }
        self.resize_pending = Some(Instant::now());
    }

    /// Request a redraw from winit.
    fn request_redraw(&self) {
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    fn has_pending_output_frames(&self) -> bool {
        self.panes.values().any(|pane| !pane.pending_output_frames.is_empty())
    }

    fn terminal_size_for_grid(&self, grid: GridSize) -> Option<TerminalSize> {
        let gpu = self.gpu.as_ref()?;
        let cell = gpu.renderer.cell_size();
        Some(TerminalSize {
            cols: grid.cols,
            rows: grid.rows,
            cell_width: round_positive_f32_to_u16(cell.width),
            cell_height: round_positive_f32_to_u16(cell.height),
        })
    }

    fn notify_pre_present(&self) {
        if let Some(window) = &self.window {
            window.pre_present_notify();
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

    /// Send any pending resize IPC message.
    ///
    /// Called once per `about_to_wait` (per-tick batching) and before any
    /// input bytes are queued so the server sees `Resize` ahead of `KeyInput`
    /// in mpsc channel order — the kernel then delivers `SIGWINCH` to the
    /// foreground process before the bytes reach the PTY. Matches the
    /// alacritty/ghostty/wezterm/kitty/vte pattern of immediate ioctl with
    /// implicit per-tick coalescing rather than a wall-clock debounce.
    fn flush_resize_if_pending(&mut self) {
        if self.resize_pending.is_some() {
            self.flush_resize_now();
        }
    }

    /// Send pending resize IPC messages immediately.
    fn flush_resize_now(&mut self) {
        let Some(tx) = &self.cmd_tx else {
            self.resize_pending = None;
            return;
        };
        let tx = tx.clone();
        let cell_size = self.gpu.as_ref().map(|gpu| {
            let cell = gpu.renderer.cell_size();
            (round_positive_f32_to_u16(cell.width), round_positive_f32_to_u16(cell.height))
        });
        for pane in self.panes.values_mut() {
            sync_pane_resize_state(pane, &tx, cell_size);
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
            let _ = self.restore_geometry_from_registry(event_loop, window_id);
        }

        // Only the bootstrap client (launched without --window-id) spawns
        // children for other unconnected windows.  Children spawned with
        // --window-id must NOT fan out — by the time they connect, some
        // siblings may not yet be registered in connected_clients, causing
        // them to appear in other_windows and get spawned a second time.
        if !self.launch.explicit_new_window {
            for &other_wid in other_windows {
                spawn_client_process(other_wid);
            }
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
    ///
    /// Returns the geometry that was actually applied so callers can use it
    /// as a stable hint for the eventual window viewport.
    /// `request_inner_size` and `set_maximized(true)` are async on most
    /// compositors, so `window.inner_size()` is not a reliable source of
    /// truth in the same synchronous block — see [[lat.md/client#Window State]].
    fn restore_geometry_from_registry(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
    ) -> Option<window_state::WindowGeometry> {
        let loaded = self.window_registry.load(window_id);
        let default = window_state::WindowGeometry::default();
        let has_saved = loaded.x.is_some()
            || loaded.y.is_some()
            || loaded.maximized
            || loaded.monitor_name.is_some()
            || loaded.width != default.width
            || loaded.height != default.height;
        let geom =
            if has_saved { Some(loaded) } else { self.window_registry.migrate_legacy(window_id) };
        let (Some(geom), Some(window)) = (geom, &self.window) else { return None };
        if window_state::apply_window_geometry(event_loop, window, &geom) {
            Some(geom)
        } else {
            None
        }
    }

    /// Spawn a new window as a separate OS process.
    fn handle_new_window(&mut self) {
        let new_id = WindowId::new();
        spawn_client_process(new_id);
        tracing::info!(current_window = ?self.window_id, %new_id, "spawning new window");
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

        if self.pending_shutdown.is_some() {
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
        let dialog = match (&self.update_progress, &self.update_available) {
            (Some(UpdateProgressState::CompletedRestartRequired { version }), _) => {
                update_dialog::UpdateDialog::new_restart_required(version.clone())
            }
            (_, Some(version)) => update_dialog::UpdateDialog::new_install(version.clone()),
            _ => return,
        };
        self.update_dialog = Some(dialog);
        self.request_redraw();
    }

    /// Process an [`update_dialog::UpdateAction`] from the in-app update dialog.
    fn handle_update_action(&mut self, action: update_dialog::UpdateAction) {
        let Some(kind) = self.update_dialog.as_ref().map(update_dialog::UpdateDialog::kind) else {
            return;
        };

        match (kind, action) {
            (
                update_dialog::UpdateDialogKind::InstallAvailable,
                update_dialog::UpdateAction::Primary,
            ) => {
                tracing::info!("user confirmed update");
                self.update_dialog = None;
                self.update_available = None;
                self.status_bar_update_rect = None;
                self.update_window_title();
                if let Some(tx) = &self.cmd_tx {
                    send_command(tx, ClientCommand::TriggerUpdate);
                }
            }
            (
                update_dialog::UpdateDialogKind::InstallAvailable,
                update_dialog::UpdateAction::Secondary,
            ) => {
                tracing::info!("user dismissed update");
                self.update_dialog = None;
                self.update_available = None;
                self.update_progress = None;
                self.status_bar_update_rect = None;
                self.update_window_title();
                if let Some(tx) = &self.cmd_tx {
                    send_command(tx, ClientCommand::DismissUpdate);
                }
            }
            (
                update_dialog::UpdateDialogKind::RestartRequired,
                update_dialog::UpdateAction::Primary,
            ) => match spawn_update_restart_helper() {
                Ok(()) => {
                    tracing::info!("user approved deferred cold restart");
                    self.update_dialog = None;
                    self.update_window_title();
                }
                Err(error) => {
                    tracing::warn!(%error, "failed to spawn deferred update restart helper");
                    return;
                }
            },
            (
                update_dialog::UpdateDialogKind::RestartRequired,
                update_dialog::UpdateAction::Secondary,
            ) => {
                tracing::info!("user postponed deferred cold restart");
                self.update_dialog = None;
                self.update_window_title();
            }
        }
        self.request_redraw();
    }

    /// Update the compositor window title to reflect the current update state.
    fn update_window_title(&self) {
        if let Some(window) = &self.window {
            let window_title = current_identity().window_title_name();
            let title = match (&self.update_progress, &self.update_available) {
                (Some(UpdateProgressState::CompletedRestartRequired { version }), _)
                    if self.status_bar_update_rect.is_some() =>
                {
                    format!("{window_title} - v{version} installed - click below to restart")
                }
                (Some(UpdateProgressState::CompletedRestartRequired { version }), _) => {
                    format!("{window_title} - v{version} installed - restart required")
                }
                (_, Some(version)) if self.status_bar_update_rect.is_some() => {
                    format!("{window_title} - v{version} available - click below to update")
                }
                (_, Some(version)) => format!("{window_title} - v{version} available"),
                _ => window_title.to_owned(),
            };
            window.set_title(&title);
        }
    }

    /// Handle keyboard input while the update dialog is active.
    fn handle_update_dialog_keyboard(&mut self, event: &winit::event::KeyEvent) {
        use winit::keyboard::{Key, NamedKey};

        if event.state != winit::event::ElementState::Pressed {
            return;
        }

        match event.logical_key {
            Key::Named(NamedKey::Escape) => {
                let action = update_dialog::UpdateAction::Secondary;
                self.handle_update_action(action);
            }
            Key::Named(NamedKey::Enter) => {
                let action = self.update_dialog.as_ref().map_or(
                    update_dialog::UpdateAction::Secondary,
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

    /// Handle `ServerDisconnected` — persist state unless a permanent window
    /// close or quit was already processed, then exit.
    fn handle_server_disconnected(&mut self, event_loop: &ActiveEventLoop) {
        tracing::info!("server disconnected, exiting");
        self.connection.server_connected = false;
        if let Some(PendingShutdown::CloseWindow { window_id }) = self.pending_shutdown {
            self.window_registry.remove(window_id);
            self.clear_restore_state();
        } else if self.connection.quit_restore_cleared {
            // QuitRequested already cleared restore state — the server
            // socket closing after the quit ack is expected, not a crash.
            tracing::debug!("skipping restore save — quit already handled");
        } else {
            // Server crashed — preserve restore state for cold restart
            // and flush any pending snapshot.
            self.flush_restore_now();
            self.flush_geometry_now();
        }
        self.pending_shutdown = None;
        self.request_redraw();
        event_loop.exit();
    }

    /// Handle `QuitRequested` from the server — save state and close.
    ///
    /// Does NOT re-broadcast `QuitAll`: only the originating window
    /// (via `handle_quit_all`) sends that.
    fn handle_quit_requested(&mut self, event_loop: &ActiveEventLoop) {
        tracing::info!("quit requested by server — saving and exiting");
        match self.pending_shutdown {
            Some(PendingShutdown::CloseWindow { window_id }) => {
                self.window_registry.remove(window_id);
            }
            _ => self.flush_geometry_now(),
        }
        self.clear_restore_state();
        self.connection.quit_restore_cleared = true;
        self.pending_shutdown = None;
        self.shutdown_notification_dispatcher();
        event_loop.exit();
    }

    /// User chose "Quit Scribe" — ask the server to broadcast a quit ack,
    /// then exit when that ack arrives.
    fn handle_quit_all(&mut self, event_loop: &ActiveEventLoop) {
        if self.pending_shutdown.is_some() {
            return;
        }

        tracing::info!("quit all — awaiting server acknowledgment");
        if let Some(tx) = &self.cmd_tx {
            self.pending_shutdown = Some(PendingShutdown::QuitAll);
            send_command(tx, ClientCommand::QuitAll);
            quit_settings_process();
            return;
        }

        quit_settings_process();
        self.flush_geometry_now();
        self.clear_restore_state();
        event_loop.exit();
    }

    /// User chose "Close this window only" — tell the server to destroy all
    /// sessions belonging to this window, then exit when the ack arrives.
    fn handle_close_window(&mut self, event_loop: &ActiveEventLoop) {
        if self.pending_shutdown.is_some() {
            return;
        }

        tracing::info!("closing window permanently — awaiting server acknowledgment");
        // Tell the server to destroy all sessions owned by this window so
        // they don't get resurrected on the next launch.
        if let Some(wid) = self.window_id {
            if let Some(tx) = &self.cmd_tx {
                self.pending_shutdown = Some(PendingShutdown::CloseWindow { window_id: wid });
                send_command(tx, ClientCommand::CloseWindow { window_id: wid });
                return;
            }
            self.window_registry.remove(wid);
        }
        self.shutdown_notification_dispatcher();
        event_loop.exit();
    }

    /// Handle `WindowClosed` from the server and complete a permanent close.
    fn handle_window_closed(&mut self, window_id: WindowId, event_loop: &ActiveEventLoop) {
        match self.pending_shutdown {
            Some(PendingShutdown::CloseWindow { window_id: pending }) if pending == window_id => {
                tracing::info!(%window_id, "window close acknowledged by server");
                self.window_registry.remove(window_id);
                self.clear_restore_state();
                self.pending_shutdown = None;
                self.shutdown_notification_dispatcher();
                event_loop.exit();
            }
            _ => {
                tracing::debug!(%window_id, "ignoring unexpected WindowClosed ack");
            }
        }
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

    // -- Restore state persistence ---------------------------------------------

    /// Mark that the window layout has changed and a restore snapshot should
    /// be saved after the debounce interval.
    fn mark_restore_dirty(&mut self) {
        if self.restore_save_pending.is_none() {
            self.restore_save_pending = Some(Instant::now());
        }
    }

    /// Flush restore state to disk if the debounce interval has elapsed.
    fn flush_restore_if_due(&mut self) {
        if self.restore_save_pending.is_some_and(|t| t.elapsed() >= RESTORE_DEBOUNCE) {
            self.flush_restore_now();
        }
    }

    /// Immediately persist the current window layout as a restore snapshot.
    fn flush_restore_now(&mut self) {
        let Some(wid) = self.window_id else { return };
        let snapshot =
            restore_replay::snapshot_window_restore(wid, &self.window_layout, &self.panes);
        if !snapshot.is_replayable() {
            if let Err(e) = self.restore_store.remove_from_index(wid) {
                tracing::warn!("failed to remove empty window from restore index: {e}");
            }
            self.restore_store.remove_window(wid);
            self.restore_save_pending = None;
            return;
        }
        if let Err(e) = self.restore_store.save_window(&snapshot) {
            tracing::warn!("failed to persist restore state: {e}");
            self.restore_save_pending = None;
            return;
        }
        if let Err(e) = self.restore_store.upsert_index(wid) {
            tracing::warn!("failed to update restore index: {e}");
        }
        self.restore_save_pending = None;
    }

    /// Remove this window's restore state from disk (explicit close/quit).
    fn clear_restore_state(&mut self) {
        let Some(wid) = self.window_id else { return };
        if let Err(e) = self.restore_store.remove_from_index(wid) {
            tracing::warn!("failed to remove window from restore index: {e}");
        }
        self.restore_store.remove_window(wid);
    }

    /// Start the animation timer thread for AI state pulsing.
    fn start_animation_timer(&mut self) {
        if self.animation.running {
            return;
        }
        self.animation.running = true;
        self.last_tick = Instant::now();

        let Some(proxy) = self.animation_proxy.clone() else { return };
        let generation = Arc::clone(&self.animation.generation);
        let thread_generation = generation.fetch_add(1, Ordering::Relaxed) + 1;
        std::thread::spawn(move || run_animation_loop(&proxy, &generation, thread_generation));
    }
}

// ---------------------------------------------------------------------------
// Animation timer
// ---------------------------------------------------------------------------

/// Run the 30 fps animation loop, sending `AnimationTick` events to the
/// winit event loop until it closes.
fn run_animation_loop(
    proxy: &EventLoopProxy<UiEvent>,
    generation: &AtomicU64,
    thread_generation: u64,
) {
    loop {
        std::thread::sleep(std::time::Duration::from_millis(33));
        if generation.load(Ordering::Relaxed) != thread_generation {
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

fn collect_session_ids(sessions: &[scribe_common::protocol::SessionInfo]) -> Vec<SessionId> {
    sessions.iter().map(|info| info.session_id).collect()
}

fn build_session_metadata_map(
    sessions: &[scribe_common::protocol::SessionInfo],
) -> SessionMetadataMap<'_> {
    sessions
        .iter()
        .map(|info| {
            (
                info.session_id,
                (
                    info.title.as_deref(),
                    info.codex_task_label.as_deref(),
                    info.cwd.as_ref(),
                    info.context.as_ref(),
                    Some(info.shell_name.as_str()),
                    info.ai_state.as_ref().map(|state| state.provider).or(info.ai_provider_hint),
                    info.ai_state.as_ref().and_then(|state| state.conversation_id.as_deref()),
                ),
            )
        })
        .collect()
}

fn group_sessions_by_workspace(
    sessions: &[scribe_common::protocol::SessionInfo],
) -> HashMap<WorkspaceId, Vec<SessionId>> {
    let mut groups: HashMap<WorkspaceId, Vec<SessionId>> = HashMap::new();
    for info in sessions {
        groups.entry(info.workspace_id).or_default().push(info.session_id);
    }
    groups
}

fn ordered_workspace_tabs(
    window_layout: &WindowLayout,
    groups: &HashMap<WorkspaceId, Vec<SessionId>>,
) -> Vec<(WorkspaceId, Vec<SessionId>)> {
    window_layout
        .workspace_ids_in_order()
        .iter()
        .filter_map(|&ws_id| groups.get(&ws_id).map(|session_ids| (ws_id, session_ids.clone())))
        .collect()
}

fn build_tab_restore_list(
    tabs_by_ws: &[(WorkspaceId, Vec<SessionId>)],
    ws_rects_map: &HashMap<WorkspaceId, Rect>,
    fallback_viewport: Rect,
) -> Vec<(WorkspaceId, SessionId, Rect, usize)> {
    tabs_by_ws
        .iter()
        .flat_map(|(ws_id, session_ids)| {
            let ws_rect = ws_rects_map.get(ws_id).copied().unwrap_or(fallback_viewport);
            let tab_count = session_ids.len().max(1);
            session_ids.iter().map(move |&session_id| (*ws_id, session_id, ws_rect, tab_count))
        })
        .collect()
}

fn is_codex_session(info: &scribe_common::protocol::SessionInfo) -> bool {
    info.ai_state.as_ref().map(|state| state.provider).or(info.ai_provider_hint)
        == Some(AiProvider::CodexCode)
}

/// Apply stored title, Codex task label, and CWD from a metadata lookup to a
/// newly created pane during reconnection.
fn apply_session_metadata(pane: &mut Pane, metadata: &SessionMetadataMap<'_>) {
    if let Some(&(title, task_label, cwd, context, shell_name, _provider, _conversation_id)) =
        metadata.get(&pane.session_id)
    {
        if let Some(title) = title {
            if !title.trim().is_empty() {
                title.clone_into(&mut pane.title);
            }
        }
        if let Some(task_label) = task_label {
            if !task_label.trim().is_empty() {
                pane.codex_task_label = Some(task_label.to_owned());
            }
        }
        if let Some(cwd) = cwd {
            pane.cwd = Some((*cwd).clone());
        }
        if let Some(context) = context {
            pane.session_context = Some((*context).clone());
        }
        if let Some(shell_name) = shell_name {
            if !shell_name.trim().is_empty() {
                shell_name.clone_into(&mut pane.shell_name);
            }
        }
    }
}

fn quote_path_for_shell(path: &std::path::Path, shell_name: &str) -> String {
    let text = path.to_string_lossy();
    match shell_name {
        "fish" => quote_fish_string(text.as_ref()),
        "pwsh" | "powershell" => quote_powershell_string(text.as_ref()),
        "nu" => quote_nushell_string(text.as_ref()),
        _ => quote_posix_string(text.as_ref()),
    }
}

fn quote_posix_string(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn quote_fish_string(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('\'', "\\'");
    format!("'{escaped}'")
}

fn quote_powershell_string(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn quote_nushell_string(value: &str) -> String {
    if !value.contains('\'') {
        return format!("'{value}'");
    }

    for hashes in 1..=8 {
        let marker = "#".repeat(hashes);
        let closing = format!("'{marker}");
        if !value.contains(&closing) {
            return format!("r{marker}'{value}'{marker}");
        }
    }

    let escaped = value.replace('\\', "\\\\").replace('\"', "\\\"");
    format!("\"{escaped}\"")
}

// ---------------------------------------------------------------------------
// Instance compositing
// ---------------------------------------------------------------------------

/// Compute the pixel X range `(start, end)` of the active tab on row 0 of the tab bar.
///
/// Returns `None` when the active tab is on a row other than row 0 (multi-row bar),
/// when there are no tabs, or when the cell width is zero.
#[derive(Clone, Copy)]
struct ActiveTabPixelRangeRequest {
    ws_rect_x: f32,
    active_tab_idx: usize,
    tab_width: u16,
    badge_cols: usize,
    tabs_per_row: usize,
    cell_w: f32,
}

fn compute_active_tab_pixel_range(request: ActiveTabPixelRangeRequest) -> Option<(f32, f32)> {
    let ActiveTabPixelRangeRequest {
        ws_rect_x,
        active_tab_idx,
        tab_width,
        badge_cols,
        tabs_per_row,
        cell_w,
    } = request;
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
    Some((
        ws_rect_x + main_grid_pixels(start_col, cell_w),
        ws_rect_x + main_grid_pixels(end_col, cell_w),
    ))
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
/// Minimum time the splash screen stays visible, ensuring the compositor
/// presents it before the terminal content frame overwrites it.  On X11,
/// `request_redraw` does not respect vsync pacing, so without a floor the
/// splash and content frames can both land in the same vsync window and
/// only the content frame is ever displayed.
const MIN_SPLASH_DURATION: Duration = Duration::from_millis(50);

/// Cursor blink interval (530ms matches xterm/VTE).
const BLINK_INTERVAL: Duration = Duration::from_millis(530);

/// Maximum number of search matches requested per query.
const SEARCH_RESULT_LIMIT: u32 = 256;

/// Debounce interval for geometry saves (move/resize events fire rapidly).
const GEOMETRY_DEBOUNCE: Duration = Duration::from_millis(500);

/// Debounce interval for restore snapshot saves.
const RESTORE_DEBOUNCE: Duration = Duration::from_millis(500);

/// Dimming factor applied to RGB channels of unfocused pane content.
const UNFOCUSED_DIM: f32 = 0.50;

/// Maximum fraction of pane width occupied by a pane title pill.
const PANE_PILL_MAX_FRACTION: f32 = tab_bar::PILL_MAX_WIDTH_FRACTION;

/// Collect all cell instances (tab bars + terminals + dividers + AI borders)
/// into one buffer.
/// `(workspace_id, tab_index, clickable_rect)` for tab bar click handling.
type TabHitTargets = Vec<(WorkspaceId, usize, layout::Rect)>;

/// `(workspace_id, equalize_rect)` for tab bar equalize button click handling.
type TabEqualizeTargets = Vec<(WorkspaceId, layout::Rect)>;

/// Tooltip anchors from all tab bars (updated each frame).
type TabTooltipTargets = Vec<tooltip::TooltipAnchor>;

struct PromptBarLayoutState {
    cell_size: (f32, f32),
    at_top: bool,
    enabled: bool,
    hover: Option<(PaneId, prompt_bar::PromptBarHover)>,
    pressed: Option<(PaneId, prompt_bar::PromptBarHover)>,
}

struct ScrollPinLayoutState {
    enabled: bool,
    hover: Option<PaneId>,
}

/// Layout and focus state passed to [`build_all_instances`].
struct FrameLayout<'a> {
    pane_rects: &'a [(PaneId, Rect)],
    dividers: &'a [divider::Divider],
    ws_dividers: &'a [workspace_layout::WorkspaceDivider],
    ws_tab_bar_data: &'a [tab_bar::WorkspaceTabBarData],
    cell_size: (f32, f32),
    focused_pane: PaneId,
    padding: &'a ContentPadding,
    scale_factor: f32,
    focused_ws_rect: Option<Rect>,
    prompt_bar: PromptBarLayoutState,
    scroll_pin: ScrollPinLayoutState,
}

/// Colors and visual styling passed to [`build_all_instances`].
struct FrameStyle<'a> {
    border_colors: &'a HashMap<PaneId, [f32; 4]>,
    tab_colors: &'a tab_bar::TabBarColors,
    divider_color: [f32; 4],
    accent_color: [f32; 4],
    focus_border_color: [f32; 4],
    focus_border_width: f32,
    scrollbar_width: f32,
    scrollbar_color: [f32; 4],
    indicator_height: f32,
    prompt_bar_colors: prompt_bar::PromptBarColors,
}

/// Interaction state passed to [`build_all_instances`].
struct FrameInteraction<'a> {
    cursor_visible: bool,
    tab_width: u16,
    active_selection: Option<&'a selection::SelectionRange>,
    search_matches: &'a [SearchMatch],
    search_current_index: usize,
    hovered_tab_close: Option<(WorkspaceId, usize)>,
    hovered_tab: Option<(WorkspaceId, usize)>,
    tab_drag: Option<&'a TabDrag>,
    tab_drag_offsets: &'a [f32],
}

struct FrameStatusSnapshot {
    focused_pane_cwd: Option<std::path::PathBuf>,
    focused_pane_git: Option<String>,
    focused_pane_display_context: Option<(String, Option<String>)>,
    focused_ws_name: Option<String>,
    session_count: usize,
}

struct PreparedFrame {
    full_viewport: Rect,
    cell_size: (f32, f32),
    pane_rects: Vec<(PaneId, Rect)>,
    dividers: Vec<divider::Divider>,
    ws_dividers: Vec<workspace_layout::WorkspaceDivider>,
    ws_tab_bar_data: Vec<tab_bar::WorkspaceTabBarData>,
    focused_pane: PaneId,
    border_colors: HashMap<PaneId, [f32; 4]>,
    focused_ws_rect: Option<Rect>,
    ws_tab_bar_heights: HashMap<WorkspaceId, f32>,
    prompt_bar_cell_size: (f32, f32),
    prompt_bar_at_top: bool,
    cursor_visible: bool,
    tab_colors: tab_bar::TabBarColors,
    sb_colors: status_bar::StatusBarColors,
    divider_color: [f32; 4],
    accent_color: [f32; 4],
    focus_border_color: [f32; 4],
    focus_border_width: f32,
    scrollbar_width: f32,
    scrollbar_color: [f32; 4],
    indicator_height: f32,
    prompt_bar_colors: prompt_bar::PromptBarColors,
    status: FrameStatusSnapshot,
}

struct WorkspaceFrameScene {
    pane_rects: Vec<(PaneId, Rect)>,
    dividers: Vec<divider::Divider>,
    ws_tab_bar_data: Vec<tab_bar::WorkspaceTabBarData>,
    focused_pane: PaneId,
}

#[derive(Clone, Copy)]
struct WorkspaceSceneContext<'a> {
    focused_ws_id: WorkspaceId,
    multi_workspace: bool,
    cell_size: (f32, f32),
    ansi_colors: &'a [[f32; 4]; 16],
}

#[derive(Clone, Copy)]
struct WorkspaceSceneEntry<'a> {
    ws_id: WorkspaceId,
    ws_rect: Rect,
    ws: &'a workspace_layout::WorkspaceSlot,
    tab: &'a workspace_layout::TabState,
}

struct PreparedFrameStyle {
    prompt_bar_cell_size: (f32, f32),
    prompt_bar_at_top: bool,
    tab_colors: tab_bar::TabBarColors,
    sb_colors: status_bar::StatusBarColors,
    divider_color: [f32; 4],
    accent_color: [f32; 4],
    focus_border_color: [f32; 4],
    focus_border_width: f32,
    scrollbar_width: f32,
    scrollbar_color: [f32; 4],
    indicator_height: f32,
    prompt_bar_colors: prompt_bar::PromptBarColors,
}

#[derive(Clone, Copy)]
struct TerminalFramePresentation {
    refresh_window_title: bool,
    request_redraw: bool,
}

type BuiltFrameInstances =
    (Vec<CellInstance>, TabHitTargets, TabHitTargets, TabEqualizeTargets, TabTooltipTargets);

struct RenderBackend<'a> {
    renderer: &'a mut TerminalRenderer,
    device: &'a wgpu::Device,
    queue: &'a wgpu::Queue,
}

impl RenderBackend<'_> {
    fn default_bg(&self) -> [f32; 4] {
        self.renderer.default_bg()
    }

    fn selection_colors(&self) -> SelectionColors {
        (self.renderer.selection_bg(), self.renderer.selection_fg())
    }

    fn resolve_glyph(&mut self, ch: char) -> ([f32; 2], [f32; 2]) {
        self.renderer.resolve_glyph(self.device, self.queue, ch)
    }

    fn build_instances_at<T: alacritty_terminal::event::EventListener>(
        &mut self,
        term: &mut alacritty_terminal::Term<T>,
        offset: (f32, f32),
        cursor_visible: bool,
    ) -> Vec<CellInstance> {
        self.renderer.build_instances_at(
            RenderResources { device: self.device, queue: self.queue },
            term,
            TerminalRenderOptions { offset, cursor_visible },
        )
    }
}

struct InstanceBuildContext<'a, 'frame> {
    backend: RenderBackend<'a>,
    panes: &'a mut HashMap<PaneId, Pane>,
    layout: &'a FrameLayout<'frame>,
    style: &'a FrameStyle<'frame>,
    interaction: &'a FrameInteraction<'frame>,
}

struct TerminalContentPassContext<'a, 'frame> {
    layout: &'a FrameLayout<'frame>,
    interaction: &'a FrameInteraction<'frame>,
    ws_tab_bar_heights: &'a HashMap<WorkspaceId, f32>,
    default_bg: [f32; 4],
    selection_colors: SelectionColors,
    effective_selection: Option<&'a selection::SelectionRange>,
    search_highlight_colors: &'a SearchHighlightColors,
}

struct ChromePassContext<'a, 'frame> {
    layout: &'a FrameLayout<'frame>,
    style: &'a FrameStyle<'frame>,
    ws_tab_bar_heights: &'a HashMap<WorkspaceId, f32>,
    has_multiple_panes: bool,
}

#[derive(Clone, Copy)]
enum PaneFocusState {
    Focused { cursor_visible: bool },
    UnfocusedDimmed,
}

impl PaneFocusState {
    fn is_focused(self) -> bool {
        matches!(self, Self::Focused { .. })
    }

    fn cursor_visible(self) -> bool {
        match self {
            Self::Focused { cursor_visible } => cursor_visible,
            Self::UnfocusedDimmed => false,
        }
    }

    fn dimmed(self) -> bool {
        matches!(self, Self::UnfocusedDimmed)
    }
}

#[derive(Clone, Copy)]
enum PaneRenderMode {
    Normal,
    SplitScroll,
}

#[derive(Clone, Copy)]
struct PaneRenderContext<'a, 'frame> {
    layout: &'a FrameLayout<'frame>,
    offset: (f32, f32),
    focus: PaneFocusState,
    mode: PaneRenderMode,
    selection: SelectionOverlayContext<'a>,
    default_bg: [f32; 4],
}

#[derive(Clone, Copy)]
struct PaneBackgroundContext {
    tab_bar_height: f32,
    prompt_bar_height: f32,
    prompt_bar_at_top: bool,
    default_bg: [f32; 4],
    dim: bool,
}

#[derive(Clone, Copy)]
struct PreparedPaneContentState<'a, 'frame> {
    background: PaneBackgroundContext,
    render: PaneRenderContext<'a, 'frame>,
    pane_sel: Option<selection::SelectionRange>,
    term_cursor_hidden: bool,
}

fn focused_workspace_rect(
    ws_rects: &WorkspaceRects,
    focused_ws_id: WorkspaceId,
    multi_workspace: bool,
) -> Option<Rect> {
    multi_workspace
        .then(|| ws_rects.iter().find(|(id, _)| *id == focused_ws_id).map(|(_, rect)| *rect))
        .flatten()
}

fn workspace_columns(ws_rect: Rect, cell_width: f32) -> usize {
    main_units_in_extent(ws_rect.width, cell_width)
}

fn prompt_bar_cell_size(
    cell_size: (f32, f32),
    prompt_bar_font_size: f32,
    font_size: f32,
) -> (f32, f32) {
    let font_scale = prompt_bar_font_size / font_size;
    (cell_size.0 * font_scale, cell_size.1 * font_scale)
}

#[derive(Clone, Copy)]
struct SplitScrollEligibility {
    scroll_pin_enabled: bool,
    ai_provider_enabled: bool,
}

impl SplitScrollEligibility {
    fn for_session(
        session_id: SessionId,
        ai_tracker: &ai_indicator::AiStateTracker,
        terminal_config: &scribe_common::config::TerminalConfig,
    ) -> Self {
        Self {
            scroll_pin_enabled: terminal_config.scroll.scroll_pin,
            ai_provider_enabled: split_scroll_provider_enabled(
                session_id,
                ai_tracker,
                terminal_config,
            ),
        }
    }

    fn allows_split_scroll(self) -> bool {
        self.scroll_pin_enabled && self.ai_provider_enabled
    }
}

fn split_scroll_provider_enabled(
    session_id: SessionId,
    ai_tracker: &ai_indicator::AiStateTracker,
    terminal_config: &scribe_common::config::TerminalConfig,
) -> bool {
    ai_tracker
        .provider_for_session(session_id)
        .is_some_and(|p| terminal_config.ai_provider_enabled(p))
}

/// Split-scroll only applies to AI panes in the normal screen buffer.
fn split_scroll_eligible(pane: &pane::Pane, eligibility: SplitScrollEligibility) -> bool {
    eligibility.allows_split_scroll()
        && pane.term.grid().display_offset() > 0
        && !pane.term.mode().contains(alacritty_terminal::term::TermMode::ALT_SCREEN)
}

/// Activate or deactivate split-scroll for a pane after a scroll operation.
fn update_split_scroll(
    pane: &mut pane::Pane,
    scroll_pin_cfg: bool,
    ai_tracker: &ai_indicator::AiStateTracker,
    terminal_config: &scribe_common::config::TerminalConfig,
) {
    let eligibility = SplitScrollEligibility {
        scroll_pin_enabled: scroll_pin_cfg,
        ai_provider_enabled: split_scroll_provider_enabled(
            pane.session_id,
            ai_tracker,
            terminal_config,
        ),
    };
    if split_scroll_eligible(pane, eligibility) {
        if pane.split_scroll.is_none() {
            pane.split_scroll = Some(split_scroll::SplitScrollState::new());
        }
    } else {
        pane.split_scroll = None;
    }
}

/// Clear stale split-scroll state after output or AI mode changes.
fn reconcile_split_scroll(pane: &mut pane::Pane, eligibility: SplitScrollEligibility) {
    if !split_scroll_eligible(pane, eligibility) {
        pane.split_scroll = None;
    }
}

fn build_all_instances(ctx: InstanceBuildContext<'_, '_>) -> BuiltFrameInstances {
    let InstanceBuildContext { mut backend, panes, layout, style, interaction } = ctx;
    let ws_tab_bar_heights: HashMap<WorkspaceId, f32> =
        layout.ws_tab_bar_data.iter().map(|d| (d.ws_id, d.tab_bar_height)).collect();
    let estimated_per_pane = 80 * 24 + 80 + 4;
    let mut all_instances = Vec::with_capacity(layout.pane_rects.len() * estimated_per_pane);
    let default_bg = backend.default_bg();
    let has_multiple_panes = layout.pane_rects.len() > 1;
    let selection_colors = backend.selection_colors();
    let search_highlight_colors = search_highlight_colors(style.accent_color);
    let content_context = TerminalContentPassContext {
        layout,
        interaction,
        ws_tab_bar_heights: &ws_tab_bar_heights,
        default_bg,
        selection_colors,
        effective_selection: interaction.active_selection.filter(|selection| !selection.is_empty()),
        search_highlight_colors: &search_highlight_colors,
    };
    build_terminal_content_pass(&mut all_instances, &mut backend, panes, &content_context);
    build_tab_bar_background_pass(&mut all_instances, layout, style);
    let (tab_hit_targets, tab_close_hit_targets, tab_equalize_targets, tab_tooltip_targets) =
        build_tab_bar_text_pass(&mut all_instances, &mut backend, layout, style, interaction);
    let chrome_context = ChromePassContext {
        layout,
        style,
        ws_tab_bar_heights: &ws_tab_bar_heights,
        has_multiple_panes,
    };
    build_chrome_passes(&mut all_instances, &mut backend, panes, &chrome_context);

    (
        all_instances,
        tab_hit_targets,
        tab_close_hit_targets,
        tab_equalize_targets,
        tab_tooltip_targets,
    )
}

fn build_chrome_passes(
    all_instances: &mut Vec<CellInstance>,
    backend: &mut RenderBackend<'_>,
    panes: &mut HashMap<PaneId, Pane>,
    context: &ChromePassContext<'_, '_>,
) {
    build_focused_workspace_border_pass(all_instances, context.layout, context.style);
    build_prompt_bar_pass(all_instances, backend, panes, context);
    build_split_scroll_chrome_pass(all_instances, panes, context);
    build_divider_pass(all_instances, context.layout, context.style);
    build_scrollbar_pass(
        all_instances,
        panes,
        context.layout,
        context.style,
        context.ws_tab_bar_heights,
    );
    build_pane_title_pass(all_instances, backend, panes, context);
    build_focused_pane_border_pass(all_instances, panes, context);
    build_ai_border_pass(
        all_instances,
        panes,
        context.layout,
        context.style,
        context.ws_tab_bar_heights,
    );
}

fn build_terminal_content_pass(
    all_instances: &mut Vec<CellInstance>,
    backend: &mut RenderBackend<'_>,
    panes: &mut HashMap<PaneId, Pane>,
    context: &TerminalContentPassContext<'_, '_>,
) {
    for (pane_id, _) in context.layout.pane_rects {
        let Some(pane) = panes.get_mut(pane_id) else { continue };
        build_terminal_content_for_pane(all_instances, backend, *pane_id, pane, context);
    }
}

fn build_terminal_content_for_pane(
    all_instances: &mut Vec<CellInstance>,
    backend: &mut RenderBackend<'_>,
    pane_id: PaneId,
    pane: &mut Pane,
    context: &TerminalContentPassContext<'_, '_>,
) {
    let prepared = prepare_pane_content_state(pane_id, pane, context);
    push_pane_bg_fill(all_instances, pane, prepared.background);
    let needs_rebuild = pane_content_needs_rebuild(pane, prepared);
    let instance_start = all_instances.len();

    if needs_rebuild {
        rebuild_pane_content(all_instances, backend, pane, prepared.render);
        cache_pane_content_state(pane, prepared);
    } else {
        all_instances.extend_from_slice(&pane.last_instances);
    }

    apply_pane_search_highlights(all_instances, instance_start, context, prepared);
}

fn prepare_pane_content_state<'a, 'frame>(
    pane_id: PaneId,
    pane: &Pane,
    context: &'a TerminalContentPassContext<'a, 'frame>,
) -> PreparedPaneContentState<'a, 'frame> {
    let tab_bar_height = pane_tab_bar_h(
        pane.workspace_id,
        context.ws_tab_bar_heights,
        context.layout.ws_tab_bar_data,
    );
    let prompt_bar_height = pane.prompt_bar_height(
        context.layout.prompt_bar.cell_size.1,
        context.layout.prompt_bar.enabled,
    );
    let content_prompt_bar_height =
        if context.layout.prompt_bar.at_top { prompt_bar_height } else { 0.0 };
    let offset = pane.content_offset(
        tab_bar_height,
        content_prompt_bar_height,
        context.layout.padding,
        context.layout.scale_factor,
    );
    let focus = if pane_id == context.layout.focused_pane {
        PaneFocusState::Focused { cursor_visible: context.interaction.cursor_visible }
    } else {
        PaneFocusState::UnfocusedDimmed
    };
    let chrome_tab_bar_height = if pane.edges.top() { tab_bar_height } else { 0.0 };
    let chrome_prompt_bar_height = if pane.edges.top() { prompt_bar_height } else { 0.0 };
    let display_offset = pane.term.grid().display_offset();
    let render_mode = if pane.split_scroll.is_some() && context.layout.scroll_pin.enabled {
        PaneRenderMode::SplitScroll
    } else {
        PaneRenderMode::Normal
    };

    PreparedPaneContentState {
        background: PaneBackgroundContext {
            tab_bar_height: chrome_tab_bar_height,
            prompt_bar_height: chrome_prompt_bar_height,
            prompt_bar_at_top: context.layout.prompt_bar.at_top,
            default_bg: context.default_bg,
            dim: focus.dimmed(),
        },
        render: PaneRenderContext {
            layout: context.layout,
            offset,
            focus,
            mode: render_mode,
            selection: SelectionOverlayContext {
                effective_selection: context.effective_selection,
                is_focused: focus.is_focused(),
                offset,
                cell_size: context.layout.cell_size,
                grid_size: pane.grid,
                selection_colors: context.selection_colors,
                display_offset,
            },
            default_bg: context.default_bg,
        },
        pane_sel: context.effective_selection.filter(|_| focus.is_focused()).copied(),
        term_cursor_hidden: !pane
            .term
            .mode()
            .contains(alacritty_terminal::term::TermMode::SHOW_CURSOR),
    }
}

fn pane_content_needs_rebuild(pane: &Pane, prepared: PreparedPaneContentState<'_, '_>) -> bool {
    pane.content_dirty
        || pane.last_cursor_visible != Some(prepared.render.focus.cursor_visible())
        || pane.last_term_cursor_hidden != Some(prepared.term_cursor_hidden)
        || pane.last_was_focused != Some(prepared.render.focus.is_focused())
        || pane.last_selection != prepared.pane_sel
        || matches!(prepared.render.mode, PaneRenderMode::SplitScroll)
}

fn cache_pane_content_state(pane: &mut Pane, prepared: PreparedPaneContentState<'_, '_>) {
    pane.content_dirty = false;
    pane.last_cursor_visible = Some(prepared.render.focus.cursor_visible());
    pane.last_term_cursor_hidden = Some(prepared.term_cursor_hidden);
    pane.last_was_focused = Some(prepared.render.focus.is_focused());
    pane.last_selection = prepared.pane_sel;
}

fn apply_pane_search_highlights(
    all_instances: &mut [CellInstance],
    instance_start: usize,
    context: &TerminalContentPassContext<'_, '_>,
    prepared: PreparedPaneContentState<'_, '_>,
) {
    if !prepared.render.focus.is_focused()
        || context.interaction.search_matches.is_empty()
        || matches!(prepared.render.mode, PaneRenderMode::SplitScroll)
    {
        return;
    }

    let Some(search_instances) = all_instances.get_mut(instance_start..) else { return };
    apply_search_match_highlight(
        search_instances,
        SearchHighlightRequest {
            offset: prepared.render.offset,
            cell_size: context.layout.cell_size,
            grid_size: prepared.render.selection.grid_size,
            matches: context.interaction.search_matches,
            current_index: context.interaction.search_current_index,
            colors: context.search_highlight_colors,
            display_offset: prepared.render.selection.display_offset,
        },
    );
}

fn rebuild_pane_content(
    all_instances: &mut Vec<CellInstance>,
    backend: &mut RenderBackend<'_>,
    pane: &mut Pane,
    context: PaneRenderContext<'_, '_>,
) {
    if matches!(context.mode, PaneRenderMode::SplitScroll) {
        rebuild_split_scroll_content(all_instances, backend, pane, context);
        return;
    }

    let mut instances =
        backend.build_instances_at(&mut pane.term, context.offset, context.focus.cursor_visible());
    let mut selection = context.selection;
    selection.display_offset = pane.term.grid().display_offset();
    apply_selection_to_instances(&mut instances, selection);
    if context.focus.dimmed() {
        dim_instances(&mut instances);
    }
    all_instances.extend_from_slice(&instances);
    std::mem::swap(&mut pane.last_instances, &mut instances);
}

fn rebuild_split_scroll_content(
    all_instances: &mut Vec<CellInstance>,
    backend: &mut RenderBackend<'_>,
    pane: &mut Pane,
    context: PaneRenderContext<'_, '_>,
) {
    let cursor_line =
        usize::try_from(pane.term.grid().cursor.point.line.0.max(0)).unwrap_or(usize::MAX);
    let screen_lines = pane.term.grid().screen_lines();
    let heuristic_pin_rows = split_scroll::compute_pin_rows(cursor_line, screen_lines);
    let prompt_pin_rows = pane.input_start.and_then(|(input_line, _)| {
        split_scroll::compute_active_prompt_pin_rows(
            pane.term.grid().history_size(),
            screen_lines,
            pane.prompt_marks.last().copied(),
            Some(input_line),
        )
    });
    let base_pin_rows =
        prompt_pin_rows.map_or(heuristic_pin_rows, |rows| rows.max(heuristic_pin_rows));
    let pin_rows =
        split_scroll::align_pin_rows_to_logical_lines(&pane.term, base_pin_rows, screen_lines);
    let pin_h = usize_to_f32(pin_rows) * context.layout.cell_size.1;
    if let Some(ss) = &mut pane.split_scroll {
        ss.pin_height = pin_h;
    }

    let content_h = usize_to_f32(screen_lines) * context.layout.cell_size.1;
    let geo = split_scroll::compute_geometry(
        layout::Rect {
            x: context.offset.0,
            y: context.offset.1,
            width: f32::from(pane.grid.cols) * context.layout.cell_size.0,
            height: content_h,
        },
        pin_h,
    );
    let saved_offset = pane.term.grid().display_offset();

    let mut top_instances = backend.build_instances_at(&mut pane.term, context.offset, false);
    let mut top_selection = context.selection;
    top_selection.display_offset = saved_offset;
    apply_selection_to_instances(&mut top_instances, top_selection);
    let top_filtered =
        filter_split_scroll_instances(&top_instances, geo.top, context.focus.dimmed());

    pane.term.scroll_display(alacritty_terminal::grid::Scroll::Bottom);
    let mut live_instances =
        backend.build_instances_at(&mut pane.term, context.offset, context.focus.cursor_visible());
    let mut live_selection = context.selection;
    live_selection.display_offset = 0;
    apply_selection_to_instances(&mut live_instances, live_selection);
    pane.term.scroll_display(alacritty_terminal::grid::Scroll::Delta(saturating_usize_to_i32(
        saved_offset,
    )));
    let bottom_filtered =
        filter_split_scroll_instances(&live_instances, geo.bottom, context.focus.dimmed());

    push_solid_rect_raw(all_instances, geo.bottom, context.default_bg);
    all_instances.extend_from_slice(&top_filtered);
    all_instances.extend_from_slice(&bottom_filtered);

    let mut composite = top_filtered;
    composite.extend_from_slice(&bottom_filtered);
    std::mem::swap(&mut pane.last_instances, &mut composite);
}

fn filter_split_scroll_instances(
    instances: &[CellInstance],
    rect: layout::Rect,
    dim: bool,
) -> Vec<CellInstance> {
    let mut filtered = split_scroll::filter_instances_by_y(instances, rect.y, rect.y + rect.height);
    if dim {
        dim_instances(&mut filtered);
    }
    filtered
}

#[derive(Clone, Copy)]
struct SelectionOverlayContext<'a> {
    effective_selection: Option<&'a selection::SelectionRange>,
    is_focused: bool,
    offset: (f32, f32),
    cell_size: (f32, f32),
    grid_size: GridSize,
    selection_colors: SelectionColors,
    display_offset: usize,
}

fn apply_selection_to_instances(
    instances: &mut [CellInstance],
    context: SelectionOverlayContext<'_>,
) {
    if let Some(selection) = context.effective_selection.filter(|_| context.is_focused) {
        apply_selection_highlight(
            instances,
            SelectionHighlightRequest {
                offset: context.offset,
                cell_size: context.cell_size,
                grid_size: context.grid_size,
                sel: selection,
                colors: context.selection_colors,
                display_offset: context.display_offset,
            },
        );
    }
}

fn build_tab_bar_background_pass(
    all_instances: &mut Vec<CellInstance>,
    layout: &FrameLayout<'_>,
    style: &FrameStyle<'_>,
) {
    for ws_data in layout.ws_tab_bar_data {
        tab_bar::build_tab_bar_bg(tab_bar::TabBarBackgroundContext {
            out: all_instances,
            rect: ws_data.ws_rect,
            colors: style.tab_colors,
            tab_bar_height: ws_data.tab_bar_height,
            active_range: ws_data.active_tab_pixel_range,
        });
    }
}

fn build_focused_workspace_border_pass(
    all_instances: &mut Vec<CellInstance>,
    layout: &FrameLayout<'_>,
    style: &FrameStyle<'_>,
) {
    if let Some(ws_rect) = layout.focused_ws_rect {
        divider::build_rect_border(
            all_instances,
            ws_rect,
            style.focus_border_color,
            style.focus_border_width,
        );
    }
}

fn build_tab_bar_text_pass(
    all_instances: &mut Vec<CellInstance>,
    backend: &mut RenderBackend<'_>,
    layout: &FrameLayout<'_>,
    style: &FrameStyle<'_>,
    interaction: &FrameInteraction<'_>,
) -> (TabHitTargets, TabHitTargets, TabEqualizeTargets, TabTooltipTargets) {
    let mut tab_hit_targets = Vec::new();
    let mut tab_close_hit_targets = Vec::new();
    let mut tab_equalize_targets = Vec::new();
    let mut tab_tooltip_targets = Vec::new();

    for ws_data in layout.ws_tab_bar_data {
        let tab_bar_rect = layout::Rect {
            x: ws_data.ws_rect.x,
            y: ws_data.ws_rect.y,
            width: ws_data.ws_rect.width,
            height: ws_data.tab_bar_height,
        };
        let badge = ws_data.badge.as_ref().map(|(name, color)| (name.as_str(), *color));
        let ws_hovered_close = interaction
            .hovered_tab_close
            .and_then(|(ws, idx)| (ws == ws_data.ws_id).then_some(idx));
        let ws_hovered_tab =
            interaction.hovered_tab.and_then(|(ws, idx)| (ws == ws_data.ws_id).then_some(idx));
        let ws_drag =
            interaction.tab_drag.filter(|drag| drag.workspace_id == ws_data.ws_id && drag.dragging);
        let ws_tab_offsets = if ws_drag.is_some() { interaction.tab_drag_offsets } else { &[] };
        let mut resolve_glyph = |ch: char| backend.resolve_glyph(ch);
        let mut params = tab_bar::TabBarTextParams {
            rect: tab_bar_rect,
            cell_size: layout.cell_size,
            tabs: &ws_data.tabs,
            badge,
            show_gear: false,
            show_equalize: ws_data.has_multiple_panes,
            colors: style.tab_colors,
            resolve_glyph: &mut resolve_glyph,
            tab_bar_height: ws_data.tab_bar_height,
            indicator_height: style.indicator_height,
            tab_width: interaction.tab_width,
            hovered_tab_close: ws_hovered_close,
            hovered_tab: ws_hovered_tab,
            tab_offsets: ws_tab_offsets,
            dragging_tab: ws_drag.map(|drag| drag.tab_index),
            drag_cursor_x: ws_drag.map_or(0.0, |drag| drag.cursor_x),
            drag_grab_offset: ws_drag.map_or(0.0, |drag| drag.grab_offset_x),
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
        tab_tooltip_targets.extend(hit_targets.tooltip_targets);

        let exact_active_range =
            tab_bar_active_range(ws_data, hit_targets.active_tab_col_range, layout.cell_size.0);
        tab_bar::build_tab_bar_separator(tab_bar::TabBarSeparatorContext {
            out: all_instances,
            rect: tab_bar_rect,
            cell_w: layout.cell_size.0,
            color: style.divider_color,
            tab_bar_height: ws_data.tab_bar_height,
            skip_range: exact_active_range,
        });
    }

    (tab_hit_targets, tab_close_hit_targets, tab_equalize_targets, tab_tooltip_targets)
}

fn tab_bar_active_range(
    ws_data: &tab_bar::WorkspaceTabBarData,
    active_tab_col_range: Option<(usize, usize)>,
    cell_w: f32,
) -> Option<(f32, f32)> {
    active_tab_col_range.map(|(start_col, end_col)| {
        (
            ws_data.ws_rect.x + main_grid_pixels(start_col, cell_w),
            ws_data.ws_rect.x + main_grid_pixels(end_col, cell_w),
        )
    })
}

fn build_prompt_bar_pass(
    all_instances: &mut Vec<CellInstance>,
    backend: &mut RenderBackend<'_>,
    panes: &HashMap<PaneId, Pane>,
    context: &ChromePassContext<'_, '_>,
) {
    for (pane_id, pane_rect) in context.layout.pane_rects {
        let Some(pane) = panes.get(pane_id) else { continue };
        if pane.prompt_count == 0 || pane.prompt_ui.dismissed || !context.layout.prompt_bar.enabled
        {
            continue;
        }
        let eff_tbh = if pane.edges.top() {
            pane_tab_bar_h(
                pane.workspace_id,
                context.ws_tab_bar_heights,
                context.layout.ws_tab_bar_data,
            )
        } else {
            0.0
        };
        let pbh = pane.prompt_bar_height(
            context.layout.prompt_bar.cell_size.1,
            context.layout.prompt_bar.enabled,
        );
        let bar_y = if context.layout.prompt_bar.at_top {
            pane_rect.y + eff_tbh
        } else {
            pane_rect.y + pane_rect.height - pbh
        };
        let glyph_size = prompt_bar_glyph_size(context.layout);
        prompt_bar::render_prompt_bar(prompt_bar::PromptBarRenderContext {
            out: all_instances,
            pane,
            bar_rect: layout::Rect {
                x: pane_rect.x,
                y: bar_y,
                width: pane_rect.width,
                height: pbh,
            },
            cell_size: context.layout.prompt_bar.cell_size,
            glyph_size,
            hover: context
                .layout
                .prompt_bar
                .hover
                .filter(|hover| hover.0 == *pane_id)
                .map(|hover| hover.1),
            active: context
                .layout
                .prompt_bar
                .pressed
                .filter(|hover| hover.0 == *pane_id)
                .map(|hover| hover.1),
            colors: &context.style.prompt_bar_colors,
            now: SystemTime::now(),
            resolve_glyph: &mut |ch| backend.resolve_glyph(ch),
        });
    }
}

fn prompt_bar_glyph_size(layout: &FrameLayout<'_>) -> [f32; 2] {
    if (layout.prompt_bar.cell_size.0 - layout.cell_size.0).abs() < 0.01
        && (layout.prompt_bar.cell_size.1 - layout.cell_size.1).abs() < 0.01
    {
        [0.0, 0.0]
    } else {
        [layout.prompt_bar.cell_size.0, layout.prompt_bar.cell_size.1]
    }
}

fn build_split_scroll_chrome_pass(
    all_instances: &mut Vec<CellInstance>,
    panes: &HashMap<PaneId, Pane>,
    context: &ChromePassContext<'_, '_>,
) {
    for (pane_id, _) in context.layout.pane_rects {
        let Some(pane) = panes.get(pane_id) else { continue };
        let Some(ss) = &pane.split_scroll else { continue };
        if !context.layout.scroll_pin.enabled || ss.pin_height <= 0.0 {
            continue;
        }
        let geo =
            split_scroll_geometry(pane, context.layout, context.ws_tab_bar_heights, ss.pin_height);
        split_scroll::render_chrome(split_scroll::SplitScrollChromeRequest {
            out: all_instances,
            geometry: &geo,
            divider_color: context.style.divider_color,
            jump_button_hovered: context.layout.scroll_pin.hover == Some(*pane_id),
            accent_color: context.style.accent_color,
        });
    }
}

fn split_scroll_geometry(
    pane: &Pane,
    layout: &FrameLayout<'_>,
    ws_tab_bar_heights: &HashMap<WorkspaceId, f32>,
    pin_height: f32,
) -> split_scroll::SplitScrollGeometry {
    let tbh = pane_tab_bar_h(pane.workspace_id, ws_tab_bar_heights, layout.ws_tab_bar_data);
    let pbh = pane.prompt_bar_height(layout.prompt_bar.cell_size.1, layout.prompt_bar.enabled);
    let content_pbh = if layout.prompt_bar.at_top { pbh } else { 0.0 };
    let content_offset = pane.content_offset(tbh, content_pbh, layout.padding, layout.scale_factor);
    let content_h = usize_to_f32(pane.term.grid().screen_lines()) * layout.cell_size.1;
    split_scroll::compute_geometry(
        layout::Rect {
            x: content_offset.0,
            y: content_offset.1,
            width: f32::from(pane.grid.cols) * layout.cell_size.0,
            height: content_h,
        },
        pin_height,
    )
}

fn build_divider_pass(
    all_instances: &mut Vec<CellInstance>,
    layout: &FrameLayout<'_>,
    style: &FrameStyle<'_>,
) {
    divider::build_divider_instances(all_instances, layout.dividers, style.divider_color);
    for ws_divider in layout.ws_dividers {
        all_instances.push(CellInstance {
            pos: [ws_divider.rect.x, ws_divider.rect.y],
            size: [ws_divider.rect.width, ws_divider.rect.height],
            uv_min: [0.0, 0.0],
            uv_max: [0.0, 0.0],
            fg_color: style.divider_color,
            bg_color: style.divider_color,
            corner_radius: 0.0,
        });
    }
}

fn build_scrollbar_pass(
    all_instances: &mut Vec<CellInstance>,
    panes: &mut HashMap<PaneId, Pane>,
    layout: &FrameLayout<'_>,
    style: &FrameStyle<'_>,
    ws_tab_bar_heights: &HashMap<WorkspaceId, f32>,
) {
    for (pane_id, _) in layout.pane_rects {
        let Some(pane) = panes.get_mut(pane_id) else { continue };
        let tbh = pane_tab_bar_h(pane.workspace_id, ws_tab_bar_heights, layout.ws_tab_bar_data);
        let eff_tbh = if pane.edges.top() { tbh } else { 0.0 };
        scrollbar::build_scrollbar_instances(
            all_instances,
            pane,
            style.scrollbar_width,
            style.scrollbar_color,
            eff_tbh,
        );
    }
}

fn build_pane_title_pass(
    all_instances: &mut Vec<CellInstance>,
    backend: &mut RenderBackend<'_>,
    panes: &HashMap<PaneId, Pane>,
    context: &ChromePassContext<'_, '_>,
) {
    if !context.has_multiple_panes {
        return;
    }

    for (pane_id, pane_rect) in context.layout.pane_rects {
        let Some(pane) = panes.get(pane_id) else { continue };
        let ws_has_multiple_panes = context
            .layout
            .ws_tab_bar_data
            .iter()
            .find(|data| data.ws_id == pane.workspace_id)
            .is_some_and(|data| data.has_multiple_panes);
        if !ws_has_multiple_panes {
            continue;
        }
        let eff_tbh = if pane.edges.top() {
            pane_tab_bar_h(
                pane.workspace_id,
                context.ws_tab_bar_heights,
                context.layout.ws_tab_bar_data,
            )
        } else {
            0.0
        };
        let mut resolve_glyph = |ch: char| backend.resolve_glyph(ch);
        tab_bar::build_pane_title_pill(tab_bar::PaneTitlePillContext {
            out: all_instances,
            title: &pane.title,
            pane_rect: *pane_rect,
            tab_bar_height: eff_tbh,
            cell_size: context.layout.cell_size,
            colors: context.style.tab_colors,
            resolve_glyph: &mut resolve_glyph,
        });
    }
}

fn build_focused_pane_border_pass(
    all_instances: &mut Vec<CellInstance>,
    panes: &HashMap<PaneId, Pane>,
    context: &ChromePassContext<'_, '_>,
) {
    if !context.has_multiple_panes {
        return;
    }

    let Some((_, focused_rect)) = context
        .layout
        .pane_rects
        .iter()
        .find(|(pane_id, _)| *pane_id == context.layout.focused_pane)
    else {
        return;
    };
    let eff_tbh = panes.get(&context.layout.focused_pane).map_or(0.0, |pane| {
        if pane.edges.top() {
            pane_tab_bar_h(
                pane.workspace_id,
                context.ws_tab_bar_heights,
                context.layout.ws_tab_bar_data,
            )
        } else {
            0.0
        }
    });
    divider::build_rect_border(
        all_instances,
        crate::layout::Rect {
            x: focused_rect.x,
            y: focused_rect.y + eff_tbh,
            width: focused_rect.width,
            height: focused_rect.height - eff_tbh,
        },
        context.style.focus_border_color,
        context.style.focus_border_width,
    );
}

fn build_ai_border_pass(
    all_instances: &mut Vec<CellInstance>,
    panes: &HashMap<PaneId, Pane>,
    layout: &FrameLayout<'_>,
    style: &FrameStyle<'_>,
    ws_tab_bar_heights: &HashMap<WorkspaceId, f32>,
) {
    for (pane_id, pane_rect) in layout.pane_rects {
        let Some(&color) = style.border_colors.get(pane_id) else { continue };
        let eff_tbh = panes.get(pane_id).map_or(0.0, |pane| {
            if pane.edges.top() {
                pane_tab_bar_h(pane.workspace_id, ws_tab_bar_heights, layout.ws_tab_bar_data)
            } else {
                0.0
            }
        });
        all_instances.extend(ai_indicator::build_border_instances(*pane_rect, color, eff_tbh));
    }
}

/// Emit one solid-colour quad covering the full pane area below the tab bar
/// and prompt bar.
///
/// Covers remainder pixels at right/bottom edges left by floor-dividing pixel
/// dimensions by cell size.  Must be pushed before cell instances so cells
/// render on top.  Applies unfocused dimming when `dim` is true.
fn push_pane_bg_fill(
    out: &mut Vec<scribe_renderer::types::CellInstance>,
    pane: &Pane,
    context: PaneBackgroundContext,
) {
    let chrome_h = context.tab_bar_height
        + if context.prompt_bar_at_top { context.prompt_bar_height } else { 0.0 };
    let total_chrome = context.tab_bar_height + context.prompt_bar_height;
    let fill_x = pane.rect.x;
    let fill_y = pane.rect.y + chrome_h;
    let fill_w = pane.rect.width.max(0.0);
    let fill_h = (pane.rect.height - total_chrome).max(0.0);
    if fill_w <= 0.0 || fill_h <= 0.0 {
        return;
    }
    let mut bg = context.default_bg;
    if context.dim {
        let [r, g, b, _] = &mut bg;
        *r *= UNFOCUSED_DIM;
        *g *= UNFOCUSED_DIM;
        *b *= UNFOCUSED_DIM;
    }
    out.push(scribe_renderer::types::CellInstance {
        pos: [fill_x, fill_y],
        size: [fill_w, fill_h],
        uv_min: [0.0, 0.0],
        uv_max: [0.0, 0.0],
        fg_color: bg,
        bg_color: bg,
        corner_radius: 0.0,
    });
}

/// Push a solid-color rectangle (no glyph, no corner radius).
fn push_solid_rect_raw(
    out: &mut Vec<scribe_renderer::types::CellInstance>,
    rect: layout::Rect,
    color: [f32; 4],
) {
    out.push(scribe_renderer::types::CellInstance {
        pos: [rect.x, rect.y],
        size: [rect.width, rect.height],
        uv_min: [0.0, 0.0],
        uv_max: [0.0, 0.0],
        fg_color: color,
        bg_color: color,
        corner_radius: 0.0,
    });
}

/// Apply window opacity to cell background alpha values.
///
/// Foreground glyphs are left fully opaque so text remains readable.
fn apply_opacity_to_instances(
    instances: &mut [scribe_renderer::types::CellInstance],
    opacity: f32,
) {
    for inst in instances {
        let [_, _, _, alpha] = &mut inst.bg_color;
        *alpha *= opacity;
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
fn dim_color(color: &mut [f32; 4]) {
    let [r, g, b, _] = color;
    *r *= UNFOCUSED_DIM;
    *g *= UNFOCUSED_DIM;
    *b *= UNFOCUSED_DIM;
}

/// Selection highlight colors: `(background, foreground)`.
type SelectionColors = ([f32; 4], [f32; 4]);
/// Search highlight colors for active (current) and passive (other) matches.
struct SearchHighlightColors {
    /// Background color for the current match (full accent).
    active_bg: [f32; 4],
    /// Foreground color for the current match (contrast against accent).
    active_fg: [f32; 4],
    /// Accent color blended with the cell's existing bg for non-current matches.
    passive_accent: [f32; 4],
}

#[derive(Clone, Copy)]
struct SelectionHighlightRequest<'a> {
    offset: (f32, f32),
    cell_size: (f32, f32),
    grid_size: GridSize,
    sel: &'a selection::SelectionRange,
    colors: SelectionColors,
    display_offset: usize,
}

#[derive(Clone, Copy)]
struct SearchHighlightRequest<'a> {
    offset: (f32, f32),
    cell_size: (f32, f32),
    grid_size: GridSize,
    matches: &'a [SearchMatch],
    current_index: usize,
    colors: &'a SearchHighlightColors,
    display_offset: usize,
}

/// Apply selection highlight to cell instances for the focused pane.
///
/// Reverse-maps each instance's pixel position to absolute grid coordinates
/// and checks whether it falls within the selection range.  Selected cells
/// get the selection background and foreground colors applied.
fn apply_selection_highlight(
    instances: &mut [scribe_renderer::types::CellInstance],
    request: SelectionHighlightRequest<'_>,
) {
    let (cell_w, cell_h) = request.cell_size;
    if cell_w <= 0.0 || cell_h <= 0.0 || request.grid_size.cols == 0 || request.grid_size.rows == 0
    {
        return;
    }
    let offset_i32 = saturating_usize_to_i32(request.display_offset);
    let max_col = usize::from(request.grid_size.cols.saturating_sub(1));
    let max_row = usize::from(request.grid_size.rows.saturating_sub(1));
    for inst in instances {
        // Skip overlay quads (beam/underline cursor) — they have non-zero size.
        if inst.size[0] != 0.0 || inst.size[1] != 0.0 {
            continue;
        }
        let col = round_main_grid_units(inst.pos[0] - request.offset.0, cell_w, max_col);
        let screen_row =
            i32::try_from(round_main_grid_units(inst.pos[1] - request.offset.1, cell_h, max_row))
                .unwrap_or(i32::MAX);
        // Convert screen row to absolute grid line to match selection coordinates.
        let grid_row = screen_row - offset_i32;
        if request.sel.contains_cell(grid_row, col) {
            inst.bg_color = request.colors.0;
            inst.fg_color = request.colors.1;
        }
    }
}

fn search_highlight_colors(accent_color: [f32; 4]) -> SearchHighlightColors {
    let luminance = 0.2126 * accent_color[0] + 0.7152 * accent_color[1] + 0.0722 * accent_color[2];
    let highlight_foreground =
        if luminance > 0.45 { [0.05, 0.05, 0.05, 1.0] } else { [0.98, 0.98, 0.98, 1.0] };
    let mut opaque_accent = accent_color;
    opaque_accent[3] = 1.0;
    SearchHighlightColors {
        active_bg: opaque_accent,
        active_fg: highlight_foreground,
        passive_accent: opaque_accent,
    }
}

/// Blend factor for passive (non-current) match backgrounds.
const PASSIVE_MATCH_BLEND: f32 = 0.4;

/// Apply search-match highlights to visible cell instances.
///
/// The current match gets the full accent background with a contrast foreground.
/// All other matches get the accent color blended into their existing background
/// at [`PASSIVE_MATCH_BLEND`] intensity, preserving the original foreground.
fn apply_search_match_highlight(
    instances: &mut [scribe_renderer::types::CellInstance],
    request: SearchHighlightRequest<'_>,
) {
    let (cell_w, cell_h) = request.cell_size;
    if cell_w <= 0.0 || cell_h <= 0.0 || request.grid_size.cols == 0 || request.grid_size.rows == 0
    {
        return;
    }

    let offset_i32 = saturating_usize_to_i32(request.display_offset);
    let max_col = usize::from(request.grid_size.cols.saturating_sub(1));
    let max_row = usize::from(request.grid_size.rows.saturating_sub(1));
    for inst in instances {
        if inst.size[0] != 0.0 || inst.size[1] != 0.0 {
            continue;
        }

        let col =
            u16::try_from(round_main_grid_units(inst.pos[0] - request.offset.0, cell_w, max_col))
                .unwrap_or(u16::MAX);
        let screen_row =
            i32::try_from(round_main_grid_units(inst.pos[1] - request.offset.1, cell_h, max_row))
                .unwrap_or(i32::MAX);
        let grid_row = screen_row - offset_i32;

        let hit = request
            .matches
            .iter()
            .enumerate()
            .find(|(_, m)| grid_row == m.row && col >= m.col_start && col <= m.col_end);
        if let Some((i, _)) = hit {
            if i == request.current_index {
                inst.bg_color = request.colors.active_bg;
                inst.fg_color = request.colors.active_fg;
            } else {
                let bg = inst.bg_color;
                let accent = request.colors.passive_accent;
                inst.bg_color = blend_search_bg(bg, accent);
            }
        }
    }
}

/// Blend a cell's existing background with the search accent at [`PASSIVE_MATCH_BLEND`] intensity.
fn blend_search_bg(bg: [f32; 4], accent: [f32; 4]) -> [f32; 4] {
    let inv = 1.0 - PASSIVE_MATCH_BLEND;
    [
        bg[0] * inv + accent[0] * PASSIVE_MATCH_BLEND,
        bg[1] * inv + accent[1] * PASSIVE_MATCH_BLEND,
        bg[2] * inv + accent[2] * PASSIVE_MATCH_BLEND,
        1.0,
    ]
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
        kind: span.kind,
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

/// URL underline color for the hovered clickable span.
const URL_UNDERLINE_ACTIVE_COLOR: [f32; 4] = [0.4, 0.8, 1.0, 1.0];

#[derive(Clone, Copy)]
enum PromptBarOverlayPosition {
    Top,
    Bottom,
}

#[derive(Clone, Copy)]
struct PromptBarOverlayState {
    enabled: bool,
    cell_h: f32,
    position: PromptBarOverlayPosition,
}

#[derive(Clone, Copy)]
struct UrlUnderlineOverlayContext<'a> {
    pane_rects: &'a [(PaneId, Rect)],
    ws_tab_bar_heights: &'a HashMap<WorkspaceId, f32>,
    fallback_tbh: f32,
    cell_size: (f32, f32),
    focused_pane_id: Option<PaneId>,
    hovered_url: Option<&'a url_detect::UrlSpan>,
    padding: &'a ContentPadding,
    scale_factor: f32,
    ctrl_held: bool,
    prompt_bar: PromptBarOverlayState,
}

/// Push a URL underline quad for the hovered clickable span in the focused
/// pane while Ctrl is held.
///
/// Refreshes the focused pane URL cache before rendering so the hover
/// highlight disappears immediately if the underlying terminal content changes.
fn apply_url_underlines(
    instances: &mut Vec<scribe_renderer::types::CellInstance>,
    url_caches: &mut HashMap<PaneId, url_detect::PaneUrlCache>,
    panes: &HashMap<PaneId, Pane>,
    context: UrlUnderlineOverlayContext<'_>,
) {
    if !context.ctrl_held {
        return;
    }
    let Some(focused_pane_id) = context.focused_pane_id else { return };
    let Some(hovered_url) = context.hovered_url else { return };
    let (cell_w, cell_h) = context.cell_size;
    if cell_w <= 0.0 || cell_h <= 0.0 {
        return;
    }
    let ul_h = URL_UNDERLINE_HEIGHT.max(1.0);

    let Some((_, _pane_rect)) =
        context.pane_rects.iter().find(|(pane_id, _)| *pane_id == focused_pane_id)
    else {
        return;
    };
    let Some(pane) = panes.get(&focused_pane_id) else { return };
    let tbh =
        context.ws_tab_bar_heights.get(&pane.workspace_id).copied().unwrap_or(context.fallback_tbh);
    let pbh = pane.prompt_bar_height(context.prompt_bar.cell_h, context.prompt_bar.enabled);
    let content_pbh = if matches!(context.prompt_bar.position, PromptBarOverlayPosition::Top) {
        pbh
    } else {
        0.0
    };
    let offset = pane.content_offset(tbh, content_pbh, context.padding, context.scale_factor);
    let display_offset = saturating_usize_to_i32(pane.term.grid().display_offset());

    let Some(cache) = url_caches.get_mut(&focused_pane_id) else { return };
    // `panes` and `url_caches` are separate parameters — no aliasing.
    cache.refresh(&pane.term);

    let Some(span) = cache.visible_spans().iter().find(|span| {
        span.row == hovered_url.row
            && span.col_start == hovered_url.col_start
            && span.col_end == hovered_url.col_end
            && span.url == hovered_url.url
            && std::mem::discriminant(&span.kind) == std::mem::discriminant(&hovered_url.kind)
    }) else {
        return;
    };

    // Convert absolute row to screen row.
    let screen_row = span.row + display_offset;
    if screen_row < 0 || span.col_start > span.col_end {
        return;
    }
    let Some(screen_row_f) = nonnegative_i32_to_f32(screen_row) else { return };
    let y_top = offset.1 + screen_row_f * cell_h + cell_h - ul_h;
    let span_cols = usize_to_f32(span.col_end - span.col_start + 1);
    let col_x = usize_to_f32(span.col_start);
    let x = offset.0 + col_x * cell_w;

    instances.push(scribe_renderer::types::CellInstance {
        pos: [x, y_top],
        size: [span_cols * cell_w, ul_h],
        uv_min: [0.0, 0.0],
        uv_max: [0.0, 0.0],
        fg_color: URL_UNDERLINE_ACTIVE_COLOR,
        bg_color: URL_UNDERLINE_ACTIVE_COLOR,
        corner_radius: 0.0,
    });
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

fn send_resize(tx: &Sender<ClientCommand>, session_id: SessionId, size: TerminalSize) {
    if tx.send(ClientCommand::Resize { session_id, size }).is_err() {
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
fn viewport_rect(config: &wgpu::SurfaceConfiguration) -> Rect {
    Rect { x: 0.0, y: 0.0, width: config.width.cast::<f32>(), height: config.height.cast::<f32>() }
}

/// Return the viewport rect available to workspaces — full surface minus
/// the window-level status bar at the bottom.
fn workspace_viewport(config: &wgpu::SurfaceConfiguration, status_bar_height: f32) -> Rect {
    Rect {
        x: 0.0,
        y: 0.0,
        width: config.width.cast::<f32>(),
        height: (config.height.cast::<f32>() - status_bar_height).max(1.0),
    }
}

/// Compute the display title for a tab in the tab bar.
///
/// Returns `"N panes"` when the tab contains multiple panes, otherwise the
/// root session's shell title (or `"tab N"` as a fallback).
fn tab_title(
    pane_count: usize,
    tab_index: usize,
    session_id: scribe_common::ids::SessionId,
    session_to_pane: &std::collections::HashMap<scribe_common::ids::SessionId, PaneId>,
    panes: &std::collections::HashMap<PaneId, pane::Pane>,
) -> String {
    if pane_count > 1 {
        return format!("{pane_count} panes");
    }
    session_to_pane
        .get(&session_id)
        .and_then(|pid| panes.get(pid))
        .map_or_else(|| format!("tab {}", tab_index + 1), |p| p.preferred_tab_title().to_owned())
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

fn history_size_delta(new_history: usize, old_history: usize) -> i32 {
    let new_history = saturating_usize_to_i32(new_history);
    let old_history = saturating_usize_to_i32(old_history);
    new_history.saturating_sub(old_history)
}

const MAX_MAIN_GRID_UNITS: usize = 65_535;

fn main_grid_units(units: usize) -> u16 {
    u16::try_from(units.min(MAX_MAIN_GRID_UNITS)).unwrap_or(u16::MAX)
}

fn main_grid_pixels(units: usize, cell_size: f32) -> f32 {
    f32::from(main_grid_units(units)) * cell_size
}

fn main_units_in_extent(extent: f32, cell_size: f32) -> usize {
    if cell_size <= 0.0 || !extent.is_finite() || extent <= 0.0 {
        return 0;
    }

    let mut low = 0usize;
    let mut high = 1usize;
    while high < MAX_MAIN_GRID_UNITS && main_grid_pixels(high, cell_size) <= extent {
        low = high;
        high = high.saturating_mul(2).min(MAX_MAIN_GRID_UNITS);
        if high == low {
            break;
        }
    }

    while low < high {
        let mid = low + (high - low).saturating_add(1) / 2;
        if main_grid_pixels(mid, cell_size) <= extent {
            low = mid;
        } else {
            high = mid.saturating_sub(1);
        }
    }

    low
}

fn round_main_grid_units(extent: f32, cell_size: f32, max_units: usize) -> usize {
    let max_units = max_units.min(MAX_MAIN_GRID_UNITS);
    if cell_size <= 0.0 || !extent.is_finite() || max_units == 0 {
        return 0;
    }

    let target = extent.max(0.0) + cell_size * 0.5;
    let mut low = 0usize;
    let mut high = max_units;
    while low < high {
        let mid = low + (high - low).saturating_add(1) / 2;
        if main_grid_pixels(mid, cell_size) <= target {
            low = mid;
        } else {
            high = mid.saturating_sub(1);
        }
    }

    low
}

fn usize_to_f32(value: usize) -> f32 {
    u32::try_from(value).unwrap_or(u32::MAX).cast::<f32>()
}

fn nonnegative_i32_to_f32(value: i32) -> Option<f32> {
    u32::try_from(value).ok().map(winit::dpi::Pixel::cast::<f32>)
}

fn round_f32_to_i32(value: f32) -> i32 {
    f64::from(value.round()).cast::<i32>()
}

fn round_positive_f32_to_u16(value: f32) -> u16 {
    f64::from(value.max(1.0).round()).cast::<u16>()
}

fn saturating_usize_to_i32(value: usize) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

/// Read the system hostname via `gethostname(2)`, falling back to "localhost".
/// Extract the X11 window ID from a winit `Window` (returns `None` on Wayland/macOS).
#[cfg(target_os = "linux")]
fn x11_window_id(window: &Window) -> Option<u32> {
    use raw_window_handle::HasWindowHandle as _;
    let handle = window.window_handle().ok()?;
    match handle.as_raw() {
        raw_window_handle::RawWindowHandle::Xlib(h) => u32::try_from(h.window).ok(),
        raw_window_handle::RawWindowHandle::Xcb(h) => Some(h.window.get()),
        _ => None,
    }
}

fn read_hostname() -> String {
    nix::unistd::gethostname().map_or_else(
        |_| String::from("localhost"),
        |hostname| hostname.to_string_lossy().into_owned(),
    )
}

/// Format the current local time as `HH:MM`.
///
/// Uses `libc::localtime_r` (the reentrant POSIX API) for timezone-aware
/// Number of prompt bar lines for a given prompt count.
/// Prompt state extracted from a cold restart snapshot for hot restart
/// reattach.  Used by `apply_snapshot_prompt_state` to avoid a complex
/// inline tuple type.
struct SnapshotPromptState {
    first: Option<String>,
    latest: Option<String>,
    latest_at: Option<SystemTime>,
    count: u32,
}

fn prompt_bar_line_count(prompt_count: u32) -> u32 {
    match prompt_count {
        0 => 0,
        1 => 1,
        _ => 2,
    }
}

/// local time.
fn current_time_str() -> String {
    chrono::Local::now().format("%H:%M").to_string()
}

use scribe_common::screen_replay::snapshot_to_ansi;

fn clear_restored_codex_snapshot_artifacts(pane: &mut Pane, is_codex: bool) {
    if is_codex {
        let _ = pane.feed_output(b"\x1b[H\x1b[2J");
    }
}

fn trim_term_scrollback(term: &mut Term<VoidListener>, kept_rows: usize, max_rows: usize) {
    let kept_rows = kept_rows.min(max_rows);
    let grid = term.grid_mut();
    grid.update_history(kept_rows);
    grid.update_history(max_rows);
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

    Some([f32::from(red) / 255.0, f32::from(green) / 255.0, f32::from(blue) / 255.0, 1.0])
}

/// Deterministic accent color for a workspace name.
///
/// Hashes the name with `DefaultHasher` ([`SipHash`]) mod palette length and
/// returns the corresponding palette color converted to linear RGBA. Returns
/// `None` when the palette is empty.
///
/// [`SipHash`]: std::collections::hash_map::DefaultHasher
fn name_to_accent_color(name: &str, palette: &[String]) -> Option<[f32; 4]> {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    if palette.is_empty() {
        return None;
    }
    let mut hasher = DefaultHasher::new();
    name.hash(&mut hasher);
    let palette_len = u64::try_from(palette.len()).ok()?;
    let bucket = hasher.finish() % palette_len;
    let index = usize::try_from(bucket).ok()?;
    let hex = palette.get(index)?;
    parse_hex_color(hex).map(scribe_renderer::srgb_to_linear_rgba)
}

/// Build the workspace badge tuple for multi-workspace mode.
///
/// Named workspaces get a deterministic accent color from the palette.
/// Unnamed workspaces return `None` — no badge is rendered.
fn make_workspace_badge(
    name: Option<&str>,
    palette: &[String],
) -> Option<(String, Option<[f32; 4]>)> {
    name.map(|n| (n.to_owned(), name_to_accent_color(n, palette)))
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
    let identity = current_identity();
    let exe = std::env::current_exe()
        .unwrap_or_else(|_| std::path::PathBuf::from(identity.client_binary_name()));
    let settings_exe = exe.with_file_name(identity.settings_binary_name());

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

    let Some(state_dir) = current_state_dir() else {
        return;
    };
    let path = state_dir.join("settings_state.toml");
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
    let exe = std::env::current_exe()
        .unwrap_or_else(|_| std::path::PathBuf::from(current_identity().client_binary_name()));
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

/// Spawn a fresh client process (no `--window-id`) for cold restart
/// multi-window restore.  The spawned process will connect, receive a new
/// window ID, and claim exactly one entry from the restore index without
/// spawning further restore children.
fn spawn_fresh_client_process() {
    let exe = std::env::current_exe()
        .unwrap_or_else(|_| std::path::PathBuf::from(current_identity().client_binary_name()));

    match std::process::Command::new(&exe).arg("--restore-child").spawn() {
        Ok(child) => {
            tracing::info!(pid = child.id(), "spawned restore window process");
        }
        Err(e) => {
            tracing::warn!(exe = %exe.display(), "failed to spawn restore window: {e}");
        }
    }
}

fn installed_client_exe_path() -> std::path::PathBuf {
    std::env::current_exe().map_or_else(
        |_| std::path::PathBuf::from(current_identity().client_binary_name()),
        |exe| exe.with_file_name(current_identity().client_binary_name()),
    )
}

fn spawn_update_restart_helper() -> Result<(), String> {
    let client_exe = installed_client_exe_path();
    let child = std::process::Command::new(&client_exe)
        .arg("--finish-update-restart")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| {
            format!("failed to spawn deferred update restart helper {}: {e}", client_exe.display())
        })?;

    tracing::info!(
        pid = child.id(),
        exe = %client_exe.display(),
        "spawned deferred update restart helper"
    );
    Ok(())
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

fn parse_restore_spawn_child() -> bool {
    std::env::args().any(|arg| arg == "--restore-child")
}

fn parse_finish_update_restart() -> bool {
    std::env::args().any(|arg| arg == "--finish-update-restart")
}

/// Walk a `WorkspaceTreeNode` tree and collect every (`SessionId` → `PaneTreeNode`)
/// pair for tabs that have split-pane layouts.
///
/// Only leaf entries with a non-`None` pane tree are included.  Single-pane tabs
/// (`None` entries in `pane_trees`) are skipped because no restore is needed.
fn extract_tab_pane_trees(
    tree: &scribe_common::protocol::WorkspaceTreeNode,
) -> HashMap<SessionId, scribe_common::protocol::PaneTreeNode> {
    let mut out = HashMap::new();
    extract_tab_pane_trees_inner(tree, &mut out);
    out
}

fn extract_tab_pane_trees_inner(
    tree: &scribe_common::protocol::WorkspaceTreeNode,
    out: &mut HashMap<SessionId, scribe_common::protocol::PaneTreeNode>,
) {
    use scribe_common::protocol::WorkspaceTreeNode;
    match tree {
        WorkspaceTreeNode::Leaf { session_ids, pane_trees, .. } => {
            for (sid, maybe_tree) in session_ids.iter().zip(pane_trees.iter()) {
                if let Some(pane_tree) = maybe_tree {
                    out.insert(*sid, pane_tree.clone());
                }
            }
        }
        WorkspaceTreeNode::Split { first, second, .. } => {
            extract_tab_pane_trees_inner(first, out);
            extract_tab_pane_trees_inner(second, out);
        }
    }
}

fn main() -> Result<(), String> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .map_or(tracing_subscriber::EnvFilter::new("info"), |filter| filter),
        )
        .init();

    if parse_finish_update_restart() {
        return ipc_client::finish_update_restart();
    }

    let event_loop = EventLoop::<UiEvent>::with_user_event()
        .build()
        .map_err(|error| format!("failed to create event loop: {error}"))?;

    let proxy = event_loop.create_proxy();

    let wgpu_instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::all(),
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });

    let window_id = parse_window_id();
    let restore_spawn_child = parse_restore_spawn_child();
    let mut app = App::new(wgpu_instance, proxy, window_id, restore_spawn_child);

    event_loop.run_app(&mut app).map_err(|error| format!("event loop exited with error: {error}"))
}

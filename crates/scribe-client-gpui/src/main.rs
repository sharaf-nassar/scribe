//! GPUI Scribe client over Scribe's frozen local IPC protocol.

mod ipc_bridge;
mod pane_shell;
mod session_lifecycle;
mod sync_frames;
mod terminal;
mod terminal_element;

use std::{
    collections::{HashMap, HashSet},
    rc::Rc,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use gpui::{
    App, AsyncApp, Bounds, Context, Entity, FocusHandle, KeyDownEvent, MouseButton, MouseDownEvent,
    Pixels, Point, Render, Size, Subscription, Task, TitlebarOptions, WeakEntity, Window,
    WindowBackgroundAppearance, WindowBounds, WindowOptions, canvas, div, prelude::*, px, relative,
    size,
};
use gpui_platform::application;
use scribe_client_gpui::ai_indicator::AiStateTracker;
use scribe_client_gpui::animation::AnimationSettings;
use scribe_client_gpui::bell::{BellController, BellEvent};
use scribe_client_gpui::chrome_metadata::{ChromeMetadata, SessionChrome};
use scribe_client_gpui::color::TerminalColors as CellColors;
use scribe_client_gpui::command_palette::{
    CommandPaletteColors, CommandPaletteEvent, CommandPaletteView, PaletteAction, build_entries,
};
use scribe_client_gpui::config::{ConfigChangeSignal, ConfigReloadPlan, ConfigRuntime};
use scribe_client_gpui::context_menu::{
    ContextMenuAction, ContextMenuColors, ContextMenuEvent, ContextMenuRequest, ContextMenuView,
    MenuItem,
};
use scribe_client_gpui::dialog::{
    AnyDialog, ClipboardDialog, CloseAction, CloseDialog, DialogColors, DialogEvent, DialogOutcome,
    DialogView, DisallowedSchemeAction, DisallowedSchemeDialog, UpdateAction, UpdateDialogKind,
};
use scribe_client_gpui::input::{self, KeyInput, TerminalMode};
use scribe_client_gpui::keybindings::{
    KeyAction, LayoutAction, OverlayChord, translate_key_action, translate_overlay_chord,
};
use scribe_client_gpui::lan::{LanChrome, LanConnectOutcome, LanEnvSummary};
use scribe_client_gpui::lan_approval::{LanApprovalAction, LanApprovalDialog};
use scribe_client_gpui::lan_dial::{self, LanDialer};
use scribe_client_gpui::layout::{FocusDirection, PaneId, Rect, SplitDirection};
use scribe_client_gpui::opacity::{clamp_opacity, opaque_slot, surface};
use scribe_client_gpui::prompt_bar::{
    self, PromptBarColors, PromptBarData, PromptContextIndicator,
};
use scribe_client_gpui::restore_replay::round_positive_f32_to_u16;
use scribe_client_gpui::search::{
    FindOverlayColors, FindOverlayEvent, FindOverlayView, FindResults, MatchHighlightColors,
    SEARCH_RESULT_LIMIT,
};
use scribe_client_gpui::share::{
    ControlRequestPrompt, ShareChrome, ShareKey, ShareKeyOutcome, ShareOverlayColors, ShareState,
    share_overlay,
};
use scribe_client_gpui::smart_selection::{
    ActionExpansionContext, ResolvedSmartSelectionAction, SmartSelectionCandidate,
};
use scribe_client_gpui::split_scroll::{SplitScrollEligibility, SplitScrollState};
use scribe_client_gpui::status_bar::{self, RemoteStatusData, StatusBarColors, StatusBarData};
use scribe_client_gpui::sys_stats::SystemStatsCollector;
use scribe_client_gpui::tooltip::{TooltipColors, TooltipPosition, TooltipRender, tooltip_element};
use scribe_client_gpui::update::UpdateState;
use scribe_client_gpui::url_detect;
use scribe_client_gpui::vi_mode::ViMotion;
use scribe_client_gpui::window_chrome;
use scribe_client_gpui::window_lifecycle::{ExitReason, FocusReport, WindowLifecycle};
use scribe_client_gpui::workspace_notes::WorkspaceNoteEntry;
use scribe_client_gpui::workspace_notes_modal::{
    WorkspaceNotesModalAction, WorkspaceNotesModalColors, WorkspaceNotesModalView,
};
use scribe_client_gpui::x11_focus::X11FocusGuard;
use scribe_client_gpui::zoom::ZoomState;
use scribe_client_gpui::{
    smart_selection::CompiledSmartSelection,
    tab_bar::{TabBarColors, context_suffix},
    tab_session::{TabEntry, TabSessions},
    titlebar::TitlebarView,
};
use scribe_common::ai_state::AiProvider;
use scribe_common::theme::ChromeColors;
use scribe_common::{
    config::{
        AiContextThresholds, SmartSelectionActionKind, SmartSelectionConfig, StatusBarStatsConfig,
        load_config,
    },
    framing::{read_message, write_message},
    ids::{SessionId, WorkspaceId},
    protocol::{
        ArchiveReason, AutomationAction, ClientMessage, ServerMessage, SessionInfo, TerminalSize,
        WindowInfo, WorkspaceNoteStatus,
    },
    screen::ScreenSnapshot,
    screen_replay::SessionReplay,
    socket::server_socket_path,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{
        Notify,
        mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel},
    },
};

use crate::{
    ipc_bridge::{InboundEvent, IpcSink, run_drain},
    pane_shell::{ClosedPane, PaneShell},
    sync_frames::{SyncFrameQueue, drain_all_committed},
    terminal::{Content, DisplayOnlyTerminal, PaneGrids, Scroll},
    terminal_element::{
        GridBounds, GridColors, GridFont, TerminalElement, cell_at, hits_jump_chip,
    },
};

/// Wall-clock origin captured at the very top of `main`, used to time
/// startup-to-first-frame for the perf A/B rig (`tools/perf-ab-rig`).
static PROCESS_START: OnceLock<Instant> = OnceLock::new();

/// Latches once the first frame has emitted its startup-timing marker so the
/// per-frame `render` hook only measures the initial paint.
static FIRST_FRAME_LOGGED: AtomicBool = AtomicBool::new(false);

/// Sentinel held by [`WINDOW_BRINGUP_MS_BITS`] until `cx.open_window` hands
/// control back to Scribe code. It is a NaN bit pattern, so it can never collide
/// with the [`f64::to_bits`] encoding of a real measurement.
const BRINGUP_UNSET: u64 = u64::MAX;

/// Milliseconds spent inside `cx.open_window` before the root-view builder runs,
/// as [`f64::to_bits`] — i.e. gpui's window creation plus wgpu adapter
/// enumeration, device creation and surface configure. No Scribe code executes
/// in that span, so it is the platform GPU bring-up floor the startup gate has
/// to account for separately from work this repo controls. See
/// `specs/016-gpui-client-rebuild/spec.md` Clarification Q3.
static WINDOW_BRINGUP_MS_BITS: AtomicU64 = AtomicU64::new(BRINGUP_UNSET);

/// Env var that opts the client into the perf-rig startup marker. Its value is
/// a file path; when set to a non-empty path, the first `render` writes the
/// machine-parseable `first_frame_ms`, `gpu_bringup_ms` and `scribe_startup_ms`
/// markers to that file. The rig reads them and gates them against the
/// re-scoped Clarification-Q3 startup budget. Unset by default so normal runs
/// write nothing.
const STARTUP_TIMING_ENV: &str = "SCRIBE_GPUI_STARTUP_TIMING";

/// Writes the startup-timing markers exactly once, on the first painted frame,
/// when [`STARTUP_TIMING_ENV`] names an output file.
///
/// `first_frame_ms` is measured from [`PROCESS_START`], so it captures the full
/// window from process launch through GPU-ready first paint — the same span the
/// old client reports through the shared probe. `gpu_bringup_ms` is the slice of
/// that span spent inside `cx.open_window` (see [`WINDOW_BRINGUP_US`]), and
/// `scribe_startup_ms` is the remainder: everything this repo actually controls.
fn log_first_frame_timing() {
    if FIRST_FRAME_LOGGED.swap(true, Ordering::AcqRel) {
        return;
    }
    let Some(path) = std::env::var_os(STARTUP_TIMING_ENV).filter(|value| !value.is_empty()) else {
        return;
    };
    let Some(start) = PROCESS_START.get() else {
        return;
    };
    let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
    let split = window_bringup_ms().map_or_else(String::new, |bringup_ms| {
        let scribe_ms = (elapsed_ms - bringup_ms).max(0.0);
        format!("gpu_bringup_ms={bringup_ms:.3}\nscribe_startup_ms={scribe_ms:.3}\n")
    });
    let marker = format!("first_frame_ms={elapsed_ms:.3}\n{split}");
    if let Err(error) = std::fs::write(&path, marker) {
        tracing::warn!(%error, "failed to write startup-timing marker");
    }
}

/// The recorded `cx.open_window` duration in milliseconds, or `None` when the
/// root view was built outside the instrumented path.
fn window_bringup_ms() -> Option<f64> {
    match WINDOW_BRINGUP_MS_BITS.load(Ordering::Acquire) {
        BRINGUP_UNSET => None,
        bits => Some(f64::from_bits(bits)),
    }
}

const COLUMNS: u16 = 120;
const ROWS: u16 = 36;

/// Label of the demo smart-selection row in the right-click context menu.
const DEMO_SMART_ACTION_LABEL: &str = "Send Text: scribe-context-menu";

/// Text the demo smart-selection row types into the attached pane. Chosen to be
/// a harmless, greppable marker so the scripted E2E can assert the click
/// actually reached the PTY.
const DEMO_SMART_ACTION_TEXT: &str = "scribe-context-menu";
const CELL_WIDTH: u16 = 8;
const CELL_HEIGHT: u16 = 18;

/// Client-side AI chrome state, written by the IPC reader and read by the GPUI
/// view on every frame.
///
/// The tracker owns the ported AI-state machine and the decoupled per-session
/// context-window store that feeds both the prompt-bar meter and the tab
/// suffix; `prompts` accumulates the `PromptReceived` history the prompt bar
/// renders. Both live behind one mutex so a frame never mixes a fresh percent
/// with a stale prompt count.
struct AiChrome {
    tracker: AiStateTracker,
    prompts: HashMap<SessionId, PromptBarData>,
}

impl AiChrome {
    /// Build the chrome state from the per-state style config that governs
    /// which AI states are tracked at all.
    fn new(styles: scribe_common::config::AiStateStylesConfig) -> Self {
        Self { tracker: AiStateTracker::new(styles), prompts: HashMap::new() }
    }

    /// Record a prompt submission for `session_id`, seeding the first-prompt row
    /// and restarting the elapsed timer on the latest row.
    fn record_prompt(&mut self, session_id: SessionId, text: String, at: std::time::SystemTime) {
        let data = self.prompts.entry(session_id).or_default();
        data.prompt_count = data.prompt_count.saturating_add(1);
        if data.first_prompt.is_none() {
            data.first_prompt = Some(text.clone());
        }
        data.latest_prompt = Some(text);
        data.latest_prompt_at = Some(at);
        data.latest_prompt_finished_at = None;
    }

    /// Drop every trace of a session, so a closed pane leaves no orphaned
    /// percentage or prompt history behind.
    fn forget(&mut self, session_id: SessionId) {
        self.tracker.remove(session_id);
        self.tracker.clear_context(session_id);
        self.prompts.remove(&session_id);
    }
}

/// Shared handles threaded from the app entry into the background IPC thread and
/// the foreground GPUI view.
#[derive(Clone)]
struct Shared {
    /// One display grid per session the window shows in a pane. The IPC drain
    /// writes each batch into the grid of the session it names; the render pass
    /// reads back the grid belonging to each pane's session.
    panes: Arc<Mutex<PaneGrids>>,
    /// Every session the client has attached, and therefore the set whose
    /// `PtyOutput` the reader lets through. A split window streams several
    /// panes at once, so this replaces the single-attached-session gate.
    attached: Arc<Mutex<HashSet<SessionId>>>,
    status: Arc<Mutex<String>>,
    generation: Arc<AtomicU64>,
    /// The session in the focused pane: what keystrokes reach and what the
    /// status bar, prompt bar, and tab-context suffix describe.
    active_session: Arc<Mutex<Option<SessionId>>>,
    /// Ordered tab strip. The IPC reader rebuilds it from `SessionList` /
    /// `SessionCreated` / `SessionExited`; the key-dispatch path moves its
    /// selection for the `next_tab` / `prev_tab` / `select_tab_N` shortcuts.
    tabs: Arc<Mutex<TabSessions>>,
    /// Server-connection flag driving the status bar's connection dot.
    connected: Arc<AtomicBool>,
    /// AI state + prompt history driving the prompt bar and the tab context %.
    ai: Arc<Mutex<AiChrome>>,
    /// Server-reported terminal chrome (CWD, git branch, session context, env
    /// health, workspace names) driving the status bar's metadata segments.
    chrome_metadata: Arc<Mutex<ChromeMetadata>>,
    /// Feature-015 share state. The IPC reader folds the roster and the control
    /// notices into it; the key path answers a pending request from it and the
    /// render pass draws its overlays and presence badge.
    share: Arc<Mutex<ShareChrome>>,
    /// Latest `UpdateAvailable` / `UpdateProgress` broadcast. The IPC reader
    /// writes it; the view renders the centred status-bar CTA from it and opens
    /// the confirmation modal a click on that CTA resolves to.
    update: Arc<Mutex<UpdateState>>,
    /// Window close / quit / focus-report state. The view raises a close, a
    /// quit, or a focus change here from a real UI event and the reader folds
    /// the server's `WindowClosed` / `QuitRequested` / `WindowList` answer back
    /// into it; the view's lifecycle tick drains the acknowledged exit.
    lifecycle: Arc<Mutex<WindowLifecycle>>,
    /// Terminal bells the IPC reader has taken off the wire and the foreground
    /// has not yet run through the [`BellController`] gate. The reader cannot
    /// touch the gate itself: it is a GPUI entity whose signal is a window-level
    /// attention request, so the reader only records the session that belled and
    /// the lifecycle tick drains the queue on the thread that owns the window.
    bells: Arc<Mutex<Vec<SessionId>>>,
    /// Latest `SearchResults` reply. The IPC reader stores it; the find
    /// overlay adopts it on the next redraw and the paint path highlights the
    /// on-screen spans it names.
    find: Arc<Mutex<FindResults>>,
    /// Feature-014 LAN state. The IPC reader parks an inbound device-approval
    /// request here and folds the peer-list, environment, and dial-gate answers
    /// into it; the view's lifecycle tick raises the parked prompt as a modal
    /// and its answer leaves through `IpcSink::lan_approval_decision`.
    lan: Arc<Mutex<LanChrome>>,
}

/// How often the foreground drains the config watcher's change signal. Short
/// enough that a saved edit lands within a frame or two, long enough that a
/// delete-and-recreate save collapses into a single reload.
const CONFIG_POLL_INTERVAL: Duration = Duration::from_millis(120);

/// How often the foreground refreshes the X11 active-window guard. Short enough
/// that a compositor overlay opening while the user is idle is noticed before
/// their next keystroke, long enough that the `_NET_ACTIVE_WINDOW` round-trip
/// stays off the hot path.
const X11_FOCUS_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// How often the foreground runs the window-lifecycle tick: draining a
/// server-acknowledged exit, reporting a focus transition the reader caused
/// (a reattach moves the focused pane without any UI event), re-polling the
/// window list when it is due, and raising a LAN device-approval prompt the
/// reader parked.
const WINDOW_LIFECYCLE_TICK: Duration = Duration::from_millis(200);

/// How often the client re-polls the server's window list. Mirrors the winit
/// client's throttle: the reply only feeds the status bar's remote-control
/// summary, so it is refreshed on a human timescale rather than per frame.
const WINDOW_LIST_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// The terminal grid's window-level background, kept in sRGB `[f32; 4]` theme
/// space until the render pass folds `appearance.opacity` into its alpha,
/// alongside the ported per-cell SGR resolver the paint path uses for every
/// colour a cell can carry.
///
/// [`CellColors`] is built once per theme rather than per frame: it linearises
/// the whole xterm-256 palette on construction, and every render hands the
/// paint path a cheap `Arc` clone of the result.
#[derive(Clone)]
struct GridPalette {
    background: [f32; 4],
    cells: Arc<CellColors>,
}

impl GridPalette {
    /// Read the grid's colours out of a resolved theme.
    fn from_theme(theme: &scribe_common::theme::Theme) -> Self {
        let mut cells = CellColors::new();
        cells.set_theme(theme);
        Self { background: theme.background, cells: Arc::new(cells) }
    }
}

struct TerminalView {
    shared: Shared,
    sink: IpcSink,
    focus_handle: FocusHandle,
    /// Live config: the resolved snapshot plus the file watcher that keeps it
    /// fresh. Held for the window's lifetime — dropping it stops the watcher.
    config: ConfigRuntime,
    /// System-stats sampler feeding the status bar's CPU/MEM/NET/GPU sparklines.
    stats: SystemStatsCollector,
    /// Theme-derived status-bar palette, rebuilt on every theme reload.
    status_colors: StatusBarColors,
    /// Which system-stat segments the status bar shows, from config.
    stats_config: StatusBarStatsConfig,
    /// Font metrics the terminal grid paints with, rebuilt on a font reload
    /// and on every zoom step.
    font: GridFont,
    /// Live font-scale step from the `zoom_in` / `zoom_out` / `zoom_reset`
    /// chords, folded into [`Self::font`] on top of `appearance.font_size`.
    zoom: ZoomState,
    /// Compiled `terminal.smart_selection` rules, rebuilt on a config reload.
    /// A right-click resolves its menu rows against these.
    smart_selection: CompiledSmartSelection,
    /// Pixel height of the split-scroll pin as of the last frame, so a click
    /// can be hit-tested against the jump chip the paint pass drew.
    split_scroll: SplitScrollState,
    /// Where the terminal grid was painted last frame, filled in by the grid
    /// canvas so a pointer position can be lowered onto a cell.
    grid_bounds: GridBounds,
    /// Pixel rect of the whole grid *area* — every pane plus the dividers
    /// between them — as of the last painted frame, recorded by the measuring
    /// canvas in [`Self::render_grid`]. This is what a pane's cell count is
    /// divided out of, so a font change re-lays the grid into the window it
    /// actually has instead of a box derived from the font itself.
    grid_area: GridBounds,
    /// Grid-area size the pane geometry was last published for, so a window
    /// resize (or a chrome band appearing) republishes exactly once.
    published_grid_area: Option<(f32, f32)>,
    /// Theme-derived prompt-bar palette, resolved once at view creation.
    prompt_colors: PromptBarColors,
    /// Warn/danger bands and per-band colours for the AI context-window meter.
    context_thresholds: AiContextThresholds,
    /// Whether `terminal.prompt_bar` is enabled in config.
    prompt_bar_enabled: bool,
    /// The window's live pane and workspace split layout. Every pane action
    /// mutates it and the render pass resolves it into the grid area's panes.
    shell: PaneShell,
    /// The grid geometry last published to the server for each pane's session,
    /// so a redraw only re-sends `Resize` when a split actually changed a
    /// pane's size.
    pane_sizes: HashMap<SessionId, TerminalSize>,
    /// Geometry of the focused pane, which is where a new tab or a split's
    /// session opens. Starts at the whole window and shrinks with the layout.
    focused_pane_size: TerminalSize,
    // The custom titlebar + integrated tab bar drawn above the terminal grid.
    titlebar: Entity<TitlebarView>,
    /// Theme chrome, retained to build the overlay palettes on demand.
    chrome: ChromeColors,
    /// Terminal dimensions announced to the server for newly created sessions.
    terminal_size: TerminalSize,
    /// Last tab strip pushed into the titlebar, so a redraw only re-renders the
    /// tab row when the shared model actually changed.
    rendered_tabs: TabSessions,
    /// Terminal background/foreground from the live theme, rebuilt on a theme
    /// reload. Replaces the hardcoded palette the spike painted with.
    terminal_colors: GridPalette,
    /// Live `appearance.opacity`, clamped to `0.0..=1.0`. Every background this
    /// window paints scales its alpha by this, so lowering it lets the desktop
    /// show through without recreating the (always transparent) window.
    opacity: f32,
    /// The command palette overlay, present only while open.
    command_palette: Option<Entity<CommandPaletteView>>,
    /// The find-in-scrollback overlay, present only while open. While it is up
    /// it owns the keyboard and its match set drives the grid's highlights.
    find_overlay: Option<Entity<FindOverlayView>>,
    /// Theme-derived find-match highlight colours, rebuilt on a theme reload.
    highlight_colors: MatchHighlightColors,
    /// The right-click context menu overlay, present only while open.
    context_menu: Option<Entity<ContextMenuView>>,
    /// The modal dialog overlay, present only while a modal is open. The spike
    /// wires two representative dialogs (close + clipboard) so the visual E2E
    /// can screenshot the ported modal chrome and its focus/button behaviour;
    /// the update confirmation on top of them is a live surface.
    dialog: Option<Entity<DialogView>>,
    /// Which update flow the open modal is confirming, so the resolved
    /// [`UpdateAction`] routes to install-vs-restart. `None` whenever the open
    /// modal is not an update confirmation.
    update_dialog_kind: Option<UpdateDialogKind>,
    /// The per-workspace notes modal overlay, present only while open.
    workspace_notes_modal: Option<Entity<WorkspaceNotesModalView>>,
    /// OSC 8 URI held while the disallowed-scheme dialog is up, so an "Open
    /// Anyway" choice can activate the verbatim URI (spec 009 FR-015). The
    /// dialog view owns its own copy for display; this is the activation copy
    /// the shell needs after the modal resolves.
    pending_osc8_uri: Option<String>,
    /// `request_id` of the LAN device approval the open modal is answering, so
    /// the resolved choice can be correlated back to the held connection. The
    /// dialog owns its own copy for display; this is the reply copy.
    pending_lan_approval: Option<u64>,
    /// Demo toggle: when set, an OSC 8-style hover tooltip is drawn over a fixed
    /// anchor so the visual E2E can exercise tooltip clamping + URL truncation.
    tooltip_demo: bool,
    /// X11 active-window guard, present only when this window has an Xcb/Xlib
    /// window id (so: X11 sessions only). Suppresses keystrokes while a
    /// compositor overlay covers the window without sending a focus event.
    x11_focus: Option<X11FocusGuard>,
    /// Terminal-bell suppression gate. The lifecycle tick feeds it the bells the
    /// IPC reader queued plus the focus context the gate reads, and the gate
    /// decides which of them are worth an attention request.
    bell: Entity<BellController>,
    /// When the window list was last polled, throttling the `ListWindows` send
    /// to [`WINDOW_LIST_POLL_INTERVAL`].
    last_window_list_poll: Instant,
    // Held to keep the redraw poll alive; dropping the view cancels the task.
    _refresh_task: Task<()>,
    /// Held to keep the config-reload poll alive; dropping the view cancels it.
    _config_task: Task<()>,
    /// Held to keep the `_NET_ACTIVE_WINDOW` poll alive; dropping the view
    /// cancels it. `None` when the focus guard is not enabled.
    _x11_focus_task: Option<Task<()>>,
    /// Held to keep the window-lifecycle tick alive; dropping the view cancels
    /// the exit drain, the focus reconciliation, and the window-list poll.
    _lifecycle_task: Task<()>,
    /// Held to keep the window-activation observer alive. It reports focus to
    /// the server on every activation change and clears the X11 guard's
    /// reactivation debounce on a genuine focus event.
    _activation_observer: Subscription,
    /// Held to keep the bell signal subscription alive. Registered with the
    /// window so a signal arrives with the `Window` its attention request needs.
    _bell_subscription: Subscription,
}

impl TerminalView {
    /// Start the X11 active-window guard from the live window.
    ///
    /// The Xcb/Xlib window id only exists once the platform window has been
    /// created, and `open_window` builds the root view with the real `Window`
    /// in hand. Non-X11 backends yield no id, leaving the guard `None` and
    /// every guard call an explicit no-op.
    fn start_x11_focus_guard(
        window: &Window,
        cx: &mut Context<Self>,
    ) -> (Option<X11FocusGuard>, Option<Task<()>>) {
        let guard = X11FocusGuard::from_window_handle(window);
        if guard.is_none() {
            return (None, None);
        }
        tracing::info!(
            window = scribe_client_gpui::x11_focus::xcb_window_id(window),
            "X11 active-window guard enabled"
        );
        let task = cx.spawn(async move |view, app| drive_x11_focus_polls(view, app).await);
        (guard, Some(task))
    }

    fn new(
        shared: Shared,
        sink: IpcSink,
        terminal_size: TerminalSize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (x11_focus, x11_focus_task) = Self::start_x11_focus_guard(window, cx);
        // The focus observer is registered unconditionally: reporting focus to
        // the server is a protocol obligation on every backend, and the X11
        // guard is only one of the two things an activation change drives.
        let activation_observer = cx
            .observe_window_activation(window, |view, window, ctx| view.on_activation(window, ctx));
        let (bell, bell_subscription) = Self::start_bell_gate(window, cx);
        // The WM's close button must raise the in-app close dialog instead of
        // destroying the window behind the server's back, so the platform close
        // is always vetoed and the dialog decides what to ask the server for.
        let close_requester = cx.weak_entity();
        window.on_window_should_close(cx, move |_window, app| {
            close_requester.update(app, TerminalView::request_window_close).unwrap_or(true)
        });
        let generation = Arc::clone(&shared.generation);
        let lifecycle_task =
            cx.spawn(async move |view, app| drive_window_lifecycle(view, app).await);
        let refresh_task =
            cx.spawn(async move |view, app| drive_redraws(view, app, generation).await);
        // Load the config and start the file watcher in one step so an edit
        // saved after this point reaches the window without a restart.
        let config = ConfigRuntime::start();
        let config_signal = config.signal();
        let config_task =
            cx.spawn(async move |view, app| drive_config_reloads(view, app, config_signal).await);
        let theme = &config.config().theme;
        let status_colors = StatusBarColors::from_theme(&theme.chrome, &theme.ansi_colors);
        let terminal_colors = GridPalette::from_theme(theme);
        let chrome = config.config().chrome;
        let opacity = clamp_opacity(config.opacity());
        let font = GridFont::from_appearance(&config.config().config.appearance);
        let stats_config = config.config().config.terminal.status_bar_stats.clone();
        let terminal = &config.config().config.terminal;
        let smart_selection = compile_smart_selection(&terminal.smart_selection);
        let context_thresholds = terminal.ai_session.context_thresholds.clone();
        let prompt_bar_enabled = terminal.prompt_bar.enabled;
        let prompt_colors = PromptBarColors::from(&chrome);
        let colors = TabBarColors::from_chrome(&chrome, opacity);
        // The strip starts empty and is filled by the reader's first
        // `SessionList`; `sync_tabs` pushes it into the titlebar on the next
        // redraw so the tab row always mirrors live server state.
        let rendered_tabs = TabSessions::new();
        let titlebar = Self::build_titlebar(colors, &rendered_tabs, cx);
        // One workspace region holding one pane: the shape the window has
        // before the first split, and the shape every split grows out of.
        let shell = PaneShell::new(chrome.accent, cx);
        Self {
            shared,
            sink,
            focus_handle: cx.focus_handle(),
            config,
            stats: SystemStatsCollector::new(),
            status_colors,
            stats_config,
            font,
            zoom: ZoomState::new(),
            smart_selection,
            split_scroll: SplitScrollState::new(),
            grid_bounds: GridBounds::default(),
            grid_area: GridBounds::default(),
            published_grid_area: None,
            prompt_colors,
            context_thresholds,
            prompt_bar_enabled,
            shell,
            pane_sizes: HashMap::new(),
            focused_pane_size: terminal_size,
            titlebar,
            chrome,
            terminal_size,
            rendered_tabs,
            terminal_colors,
            opacity,
            highlight_colors: MatchHighlightColors::from_chrome(&chrome),
            command_palette: None,
            find_overlay: None,
            context_menu: None,
            dialog: None,
            update_dialog_kind: None,
            workspace_notes_modal: None,
            pending_osc8_uri: None,
            pending_lan_approval: None,
            tooltip_demo: false,
            x11_focus,
            bell,
            last_window_list_poll: Instant::now(),
            _refresh_task: refresh_task,
            _config_task: config_task,
            _x11_focus_task: x11_focus_task,
            _lifecycle_task: lifecycle_task,
            _activation_observer: activation_observer,
            _bell_subscription: bell_subscription,
        }
    }

    /// Create the terminal-bell suppression gate and wire its signal to the
    /// window's attention request.
    ///
    /// The gate is seeded with the window's real focus state rather than the
    /// entity default, so a bell that arrives before the first activation change
    /// is judged against reality. The subscription is registered *in* the window
    /// because the only thing a signal does is call `Window::request_attention`,
    /// and `subscribe_in` is what hands the handler that window.
    fn start_bell_gate(
        window: &Window,
        cx: &mut Context<Self>,
    ) -> (Entity<BellController>, Subscription) {
        let active = window.is_window_active();
        let bell = cx.new(|_| {
            let mut controller = BellController::new();
            controller.set_window_focused(active);
            controller
        });
        let subscription = cx.subscribe_in(&bell, window, |_, _, event, window, _| {
            Self::on_bell_signal(*event, window);
        });
        (bell, subscription)
    }

    /// Build the custom titlebar seeded with `tabs`.
    ///
    /// Split out of [`Self::new`] so the constructor stays a list of the
    /// window's collaborators rather than also being the place one of them is
    /// assembled.
    fn build_titlebar(
        colors: TabBarColors,
        tabs: &TabSessions,
        cx: &mut Context<Self>,
    ) -> Entity<TitlebarView> {
        let data = tabs.to_tab_data();
        cx.new(|cx| {
            let mut bar = TitlebarView::new(colors, cx);
            bar.set_tabs(data, cx);
            bar
        })
    }

    /// Drain a pending config-file change and reapply it to the live window.
    ///
    /// Runs on the GPUI foreground (the watcher thread only bumps an atomic),
    /// so every surface below is touched from the thread that owns it.
    fn reload_config(&mut self, cx: &mut Context<Self>) {
        let Some(plan) = self.config.poll_reload() else {
            return;
        };
        self.apply_config_reload(plan, cx);
    }

    /// Reapply one reload plan: theme-derived palettes, grid font metrics, and
    /// the opacity hook, then announce the reload to the server.
    ///
    /// Keybindings need no branch here — [`ConfigRuntime`] re-parses them on
    /// every reload and both `handle_overlay_key` and [`Self::handle_binding`]
    /// read them fresh on each keystroke, so a saved shortcut edit is live
    /// immediately.
    fn apply_config_reload(&mut self, plan: ConfigReloadPlan, cx: &mut Context<Self>) {
        if plan.theme_changed() {
            let theme = self.config.config().theme.clone();
            self.status_colors = StatusBarColors::from_theme(&theme.chrome, &theme.ansi_colors);
            self.terminal_colors = GridPalette::from_theme(&theme);
            self.chrome = theme.chrome;
            self.prompt_colors = PromptBarColors::from(&self.chrome);
            self.highlight_colors = MatchHighlightColors::from_chrome(&self.chrome);
            self.push_tab_bar_colors(cx);
            // Open overlays captured the old palette when they were built; drop
            // them so a live theme edit never leaves stale colours on screen.
            self.command_palette = None;
            self.find_overlay = None;
            self.context_menu = None;
        }

        if plan.font_changed() {
            // Rebuilt through the zoom step so a saved font-size edit rebases
            // the live zoom instead of silently discarding it.
            self.rebuild_font(cx);
        }

        if plan.opacity_changed() {
            self.apply_opacity_change(cx);
        }

        // Status-bar stat selection and the prompt-bar toggles are cheap to swap
        // and have no plan flag.
        let terminal = &self.config.config().config.terminal;
        self.stats_config = terminal.status_bar_stats.clone();
        self.context_thresholds = terminal.ai_session.context_thresholds.clone();
        self.prompt_bar_enabled = terminal.prompt_bar.enabled;
        self.smart_selection = compile_smart_selection(&terminal.smart_selection);

        // Tell the server to re-read the same file so its own live surfaces
        // (clipboard policy, env store, remote/share listeners) follow.
        if let Err(error) = self.sink.config_reloaded() {
            tracing::warn!(%error, "ConfigReloaded dropped: IPC writer closed");
        }

        tracing::info!(
            theme = plan.theme_changed(),
            font = plan.font_changed(),
            opacity = plan.opacity_changed(),
            "config hot-reloaded"
        );
        cx.notify();
    }

    /// Republish every pane's grid size after a font edit changed cell metrics,
    /// then resync each pane from the server's authoritative screen.
    ///
    /// The `Resize` re-runs `TIOCSWINSZ` on the server's PTY, which raises
    /// `SIGWINCH` in the foreground process; a full-screen app answers by
    /// redrawing against the new metrics and a line-oriented shell answers with
    /// nothing at all. Either way this display-only client cannot re-derive the
    /// resulting grid — it owns no PTY and never replays locally — so the
    /// `Resize` is followed by a `RequestSnapshot` on the same ordered channel.
    /// The server answers with the post-resize per-cell grid plus scrollback,
    /// and [`dispatch_server_message`]'s `ScreenSnapshot` arm resets the pane
    /// and replays it, so the window repaints from server state instead of
    /// waiting for the next prompt.
    ///
    /// The cached sizes are cleared first, because new cell metrics change
    /// every pane's cell count even though no split moved.
    fn report_cell_metrics(&mut self, cx: &mut Context<Self>) {
        self.pane_sizes.clear();
        self.publish_pane_sizes(cx);
    }

    /// Delivery point for the reload plan's `opacity_changed()` signal.
    ///
    /// The window's native surface is always transparent (see [`open_window`]),
    /// so honouring a new opacity is purely a repaint: cache the clamped value,
    /// push it into the titlebar's cached palette, and let the `cx.notify()` at
    /// the end of the reload redraw every alpha-aware background. No restart and
    /// no window recreation is involved, unlike the legacy client which had to
    /// refuse a live change when the window was created opaque.
    fn apply_opacity_change(&mut self, cx: &mut Context<Self>) {
        self.opacity = clamp_opacity(self.config.opacity());
        self.push_tab_bar_colors(cx);
        tracing::info!(opacity = self.opacity, "config reload: opacity applied");
    }

    /// Rebuild the titlebar's cached palette from the live chrome and opacity.
    ///
    /// [`TitlebarView`] owns its colours, so both a theme edit and an opacity
    /// edit have to push a fresh palette rather than relying on the root
    /// render pass.
    fn push_tab_bar_colors(&mut self, cx: &mut Context<Self>) {
        let colors = TabBarColors::from_chrome(&self.chrome, self.opacity);
        self.titlebar.update(cx, |bar, ctx| bar.set_colors(colors, ctx));
    }

    /// Mirror the shared tab strip into the titlebar when it changed.
    ///
    /// The strip is mutated off the GPUI thread by the IPC reader, so the view
    /// reconciles it on each redraw rather than being pushed to.
    fn sync_tabs(&mut self, cx: &mut Context<Self>) {
        let Ok(tabs) = self.shared.tabs.lock() else { return };
        if *tabs == self.rendered_tabs {
            return;
        }
        self.rendered_tabs = tabs.clone();
        drop(tabs);
        let data = self.rendered_tabs.to_tab_data();
        self.titlebar.update(cx, |bar, ctx| bar.set_tabs(data, ctx));
    }

    /// Run one [`LayoutAction`] intercepted from the key path.
    ///
    /// Tab creation, selection, and closing are wired to the IPC sink, window
    /// creation opens a second top-level window, the four scrollback actions
    /// move the display viewport, the three zoom actions rescale the grid font,
    /// and the pane and workspace families drive the window's [`PaneShell`].
    /// The remaining clipboard and prompt-jump families are still swallowed
    /// (never forwarded to the PTY), matching the legacy client's behaviour of
    /// never leaking a bound shortcut as terminal bytes.
    ///
    /// The match is exhaustive and the swallowed variants are named one by one
    /// rather than folded into a `_` arm: a new [`LayoutAction`] then fails to
    /// compile here instead of silently joining the dropped set, and every drop
    /// that does happen is counted and warned by [`unhandled_layout_action`].
    ///
    /// The two split families follow the legacy client's naming: a "vertical"
    /// split draws a vertical divider and therefore places the new pane beside
    /// the old one ([`SplitDirection::Horizontal`]), and a "horizontal" split
    /// stacks them.
    fn handle_layout_action(&mut self, action: LayoutAction, cx: &mut Context<Self>) {
        match action {
            LayoutAction::SplitVertical => self.split_pane(SplitDirection::Horizontal, cx),
            LayoutAction::SplitHorizontal => self.split_pane(SplitDirection::Vertical, cx),
            LayoutAction::ClosePane => self.close_pane(cx),
            LayoutAction::FocusNext => self.focus_next_pane(cx),
            LayoutAction::FocusLeft => self.focus_pane(FocusDirection::Left, cx),
            LayoutAction::FocusRight => self.focus_pane(FocusDirection::Right, cx),
            LayoutAction::FocusUp => self.focus_pane(FocusDirection::Up, cx),
            LayoutAction::FocusDown => self.focus_pane(FocusDirection::Down, cx),
            LayoutAction::WorkspaceSplitVertical => {
                self.split_workspace(SplitDirection::Horizontal, cx);
            }
            LayoutAction::WorkspaceSplitHorizontal => {
                self.split_workspace(SplitDirection::Vertical, cx);
            }
            LayoutAction::WorkspaceFocusLeft => self.focus_workspace(FocusDirection::Left, cx),
            LayoutAction::WorkspaceFocusRight => self.focus_workspace(FocusDirection::Right, cx),
            LayoutAction::WorkspaceFocusUp => self.focus_workspace(FocusDirection::Up, cx),
            LayoutAction::WorkspaceFocusDown => self.focus_workspace(FocusDirection::Down, cx),
            LayoutAction::NewTab => self.create_tab(None),
            LayoutAction::NewClaudeTab => {
                self.create_tab(Some(ai_tab_command(AiProvider::ClaudeCode, false)));
            }
            LayoutAction::NewClaudeResumeTab => {
                self.create_tab(Some(ai_tab_command(AiProvider::ClaudeCode, true)));
            }
            LayoutAction::NewCodexTab => {
                self.create_tab(Some(ai_tab_command(AiProvider::CodexCode, false)));
            }
            LayoutAction::NewCodexResumeTab => {
                self.create_tab(Some(ai_tab_command(AiProvider::CodexCode, true)));
            }
            LayoutAction::NextTab => self.switch_tab(TabSessions::focus_next, cx),
            LayoutAction::PrevTab => self.switch_tab(TabSessions::focus_prev, cx),
            LayoutAction::SelectTab(index) => {
                self.switch_tab(move |tabs| tabs.select(index), cx);
            }
            LayoutAction::CloseTab => self.close_active_tab(),
            LayoutAction::NewWindow => self.open_new_window(cx),
            LayoutAction::ScrollUp => self.scroll_terminal(Scroll::PageUp, cx),
            LayoutAction::ScrollDown => self.scroll_terminal(Scroll::PageDown, cx),
            LayoutAction::ScrollTop => self.scroll_terminal(Scroll::Top, cx),
            LayoutAction::ScrollBottom => self.scroll_terminal(Scroll::Bottom, cx),
            LayoutAction::ZoomIn => self.apply_zoom(ZoomState::zoom_in, cx),
            LayoutAction::ZoomOut => self.apply_zoom(ZoomState::zoom_out, cx),
            LayoutAction::ZoomReset => self.apply_zoom(ZoomState::reset, cx),
            LayoutAction::CopySelection
            | LayoutAction::PasteClipboard
            | LayoutAction::PromptJumpUp
            | LayoutAction::PromptJumpDown
            | LayoutAction::JumpToFailure => unhandled_layout_action(action),
        }
    }

    /// Run `edit` against the focused pane's display grid.
    ///
    /// Every terminal-navigation surface — scrollback, vi/copy mode, the
    /// split-scroll pin, smart selection — acts on the pane the user is in,
    /// and `active_session` names that pane's session by construction: both
    /// [`Self::focus_pane_session`] and the reader's attach path re-point it
    /// on every focus move. Returns `None` when no pane is attached yet.
    fn with_focused_grid<R>(&self, edit: impl FnOnce(&mut DisplayOnlyTerminal) -> R) -> Option<R> {
        let session_id = (*self.shared.active_session.lock().ok()?)?;
        let mut grids = self.shared.panes.lock().ok()?;
        Some(edit(grids.grid_mut(session_id)))
    }

    /// Move the display viewport and repaint.
    ///
    /// A scroll away from the bottom re-evaluates the split-scroll gate, so an
    /// eligible AI pane grows its pinned live region on the very first page-up
    /// rather than a frame later.
    fn scroll_terminal(&mut self, scroll: Scroll, cx: &mut Context<Self>) {
        let Some((moved, offset, pin_rows)) = self.with_focused_grid(|terminal| {
            if matches!(scroll, Scroll::Bottom) {
                // Landing at the bottom dissolves the split by definition;
                // clearing the gate first keeps the pin from surviving one
                // extra frame.
                terminal.set_split_scroll_eligibility(SplitScrollEligibility::default());
            }
            let moved = terminal.scroll(scroll);
            (moved, terminal.display_offset(), terminal.pin_rows())
        }) else {
            return;
        };
        tracing::info!(?scroll, moved, offset, pin_rows, "terminal scrollback moved");
        cx.notify();
    }

    /// Apply one zoom step and republish the resulting cell metrics.
    ///
    /// The step is folded into the grid font rather than into the config, so a
    /// later config reload rebases the zoom on the new `appearance.font_size`
    /// instead of discarding it.
    fn apply_zoom(&mut self, step: impl FnOnce(&mut ZoomState), cx: &mut Context<Self>) {
        step(&mut self.zoom);
        self.rebuild_font(cx);
        tracing::info!(level = self.zoom.level(), size = self.font.size, "terminal zoom changed");
    }

    /// Rebuild the grid font from the live appearance config plus the current
    /// zoom step, then tell the server what the new cell box measures.
    fn rebuild_font(&mut self, cx: &mut Context<Self>) {
        let appearance = self.config.config().config.appearance.clone();
        self.font = GridFont::from_appearance(&scribe_common::config::AppearanceConfig {
            font_size: self.zoom.effective_font_size(appearance.font_size),
            ..appearance
        });
        self.report_cell_metrics(cx);
        cx.notify();
    }

    /// The single live dispatch point for a resolved [`KeyAction`].
    ///
    /// Every producer funnels through here — the keybinding path
    /// ([`Self::handle_binding`]) and the command palette
    /// ([`Self::execute_automation_action`]) — so a palette row and its bound
    /// chord can never drift apart, and wiring one surface wires both. The
    /// still-unwired family (`OpenSettings`) is named here rather than folded
    /// into a `_` arm, so a new [`KeyAction`] fails to compile instead of
    /// silently joining the dropped set.
    fn dispatch_key_action(&mut self, action: KeyAction, cx: &mut Context<Self>) {
        match action {
            KeyAction::Layout(layout) => self.handle_layout_action(layout, cx),
            KeyAction::Terminal(bytes) => self.send_key_bytes(bytes),
            KeyAction::OpenCommandPalette => self.open_command_palette(cx),
            KeyAction::OpenFind => self.open_find_overlay(cx),
            // The settings window (FU-23) is a separate bead. It is still
            // swallowed so it cannot reach the PTY, and counted so the drop is
            // visible in the log.
            KeyAction::OpenSettings => {
                unroutable_action(&format!("{action:?}"), "no handler in the GPUI shell yet");
            }
        }
    }

    /// Dispatch a confirmed command-palette row.
    ///
    /// Ports the winit `execute_palette_action` seam: shared automation actions
    /// run through [`Self::execute_automation_action`], and the feature-013
    /// client-local remote-connect row is client-only (FU-16).
    fn execute_palette_action(&mut self, action: PaletteAction, cx: &mut Context<Self>) {
        match action {
            PaletteAction::Automation(automation) => {
                self.execute_automation_action(automation, cx);
            }
            PaletteAction::OpenRemoteConnect => {
                unroutable_action("OpenRemoteConnect", "the remote-connect picker is not ported");
            }
        }
    }

    /// Run one shared [`AutomationAction`].
    ///
    /// Most rows have an exact [`KeyAction`] twin, so they are lowered by
    /// [`key_action_for_automation`] and handed to [`Self::dispatch_key_action`]
    /// — the same call the keybinding path makes. The three actions with no
    /// bindable chord are handled here: a profile switch reloads the live
    /// config in place, session focus moves the tab selection, and the update
    /// dialog waits on the update surfaces (FU-14).
    fn execute_automation_action(&mut self, action: AutomationAction, cx: &mut Context<Self>) {
        if let Some(key_action) = key_action_for_automation(&action) {
            self.dispatch_key_action(key_action, cx);
            return;
        }
        match action {
            AutomationAction::SwitchProfile { name } => self.switch_profile(&name, cx),
            AutomationAction::FocusSession { session_id } => self.focus_session(session_id, cx),
            AutomationAction::OpenUpdateDialog => {
                unroutable_action("OpenUpdateDialog", "update state is not tracked in the client");
            }
            // Everything else was lowered onto a `KeyAction` above.
            other => unroutable_action(&format!("{other:?}"), "no automation handler"),
        }
    }

    /// Activate `name` as the current profile and apply the config it wrote.
    ///
    /// [`scribe_common::profiles::switch_profile`] copies the stored profile
    /// over `config.toml` and hands back the parsed result, so the live window
    /// is reloaded from that value directly instead of racing the file watcher
    /// against its own write.
    fn switch_profile(&mut self, name: &str, cx: &mut Context<Self>) {
        match scribe_common::profiles::switch_profile(name) {
            Ok(config) => {
                let plan = self.config.reload(config);
                self.apply_config_reload(plan, cx);
                tracing::info!(profile = %name, "switched profile");
            }
            Err(error) => tracing::warn!(profile = %name, %error, "failed to switch profile"),
        }
    }

    /// Raise the tab hosting `session_id` and attach it.
    fn focus_session(&mut self, session_id: SessionId, cx: &mut Context<Self>) {
        self.switch_tab(
            move |tabs| {
                let index = tabs.tabs().iter().position(|tab| tab.session_id == session_id)?;
                tabs.select(index)
            },
            cx,
        );
    }

    /// Dispatch an action chosen from the right-click context menu.
    ///
    /// Ports the winit `dispatch_context_menu_action` routing for the open/run
    /// group: heuristic URLs keep the silent scheme-allowlist drop, an OSC 8
    /// URI goes through [`Self::route_osc8_activation`] so a disallowed scheme
    /// prompts first (spec 009 FR-003 / FR-015), file paths open with the OS
    /// handler, and the smart-selection run/send actions reach the attached
    /// pane or a fresh tab. The clipboard trio (Copy / Paste / Select All and
    /// the copy-text rows) needs the host clipboard and a live selection model,
    /// which is a separate bead; those rows are counted rather than dropped.
    fn dispatch_context_menu_action(&mut self, action: ContextMenuAction, cx: &mut Context<Self>) {
        match action {
            ContextMenuAction::OpenUrl(url) => url_detect::open_url(&url),
            ContextMenuAction::OpenOsc8Url(uri) => self.route_osc8_activation(uri, cx),
            ContextMenuAction::OpenFile(path) => url_detect::open_path(&path, None),
            // An explicit user-initiated run, not a clipboard paste: it bypasses
            // the paste-confirmation gate exactly as the legacy client does.
            ContextMenuAction::RunCommand(command) => {
                self.send_key_bytes(format!("{command}\n").into_bytes());
            }
            ContextMenuAction::SendText(text) => self.send_key_bytes(text.into_bytes()),
            ContextMenuAction::RunCommandInWindow(command) => {
                self.create_tab(Some(shell_command_argv(&command)));
            }
            ContextMenuAction::RunCoprocess(command) => spawn_background_command(&command),
            ContextMenuAction::Copy
            | ContextMenuAction::Paste
            | ContextMenuAction::SelectAll
            | ContextMenuAction::CopyText(_)
            | ContextMenuAction::CopyHyperlinkAddress(_) => {
                unroutable_action(&format!("{action:?}"), "clipboard and selection are not wired");
            }
        }
        cx.notify();
    }

    /// Route an OSC 8 activation through the scheme-allowlist gate.
    ///
    /// An allowlisted scheme opens straight away with no added latency; any
    /// other scheme raises the disallowed-scheme confirmation and parks the
    /// verbatim URI on [`Self::pending_osc8_uri`] until the modal resolves.
    fn route_osc8_activation(&mut self, uri: String, cx: &mut Context<Self>) {
        if url_detect::is_allowed_scheme(&uri) {
            url_detect::open_url(&uri);
            return;
        }
        let scheme = url_detect::extract_scheme(&uri).unwrap_or_default();
        self.pending_osc8_uri = Some(uri.clone());
        self.open_dialog(AnyDialog::DisallowedScheme(DisallowedSchemeDialog::new(uri, scheme)), cx);
    }

    /// Route a resolved modal choice.
    ///
    /// Four outcomes have a live consumer: an update decision goes to
    /// [`Self::route_update_action`], a close decision to
    /// [`Self::route_close_action`], a LAN device-approval decision to
    /// [`Self::route_lan_approval_action`], and a disallowed-scheme "Open
    /// Anyway" activates the parked URI without the allowlist guard. The paste
    /// and clipboard modals answer server prompts and are wired with those
    /// surfaces; Escape / backdrop resolves to each modal's safe action, which
    /// for the close dialog is Cancel and therefore a deliberate no-op — but for
    /// the approval prompt is an explicit Decline that still has to reach the
    /// server, because a peer is being held open waiting for it.
    fn route_dialog_outcome(&mut self, outcome: DialogOutcome) {
        // Three consumers read state the dialog parked on the view, so take the
        // pending URI and the pending approval id up front and clear the update
        // kind only once the update route (which reads it) has run.
        let pending = self.pending_osc8_uri.take();
        let approval = self.pending_lan_approval.take();
        match outcome {
            DialogOutcome::Update(action) => self.route_update_action(action),
            DialogOutcome::Close(action) => self.route_close_action(action),
            DialogOutcome::LanApproval(action) => {
                self.route_lan_approval_action(approval, action);
            }
            DialogOutcome::DisallowedScheme(DisallowedSchemeAction::OpenAnyway) => {
                if let Some(uri) = pending {
                    url_detect::open_uri_unguarded(&uri);
                }
            }
            DialogOutcome::Paste(_)
            | DialogOutcome::Clipboard(_)
            | DialogOutcome::DisallowedScheme(_) => {}
        }
        // The dialog is gone either way, so its kind must not outlive it.
        self.update_dialog_kind = None;
    }

    /// Raise the LAN device-approval prompt the IPC reader parked, if any.
    ///
    /// Runs on the foreground tick rather than from the reader thread: the
    /// prompt is a GPUI entity and only the thread that owns the window may
    /// build one. A modal already being up defers the prompt to a later tick
    /// instead of stacking two — the peer is held either way.
    fn poll_lan_approval(&mut self, cx: &mut Context<Self>) {
        if self.dialog.is_some() {
            return;
        }
        let Some(request) = self.shared.lan.lock().ok().and_then(|mut lan| lan.take_approval())
        else {
            return;
        };
        let request_id = request.request_id();
        self.pending_lan_approval = Some(request_id);
        tracing::info!(request_id, "raising the LAN device-approval prompt");
        self.open_dialog(AnyDialog::LanApproval(request), cx);
    }

    /// Answer a pending LAN device approval on the wire.
    ///
    /// The reply is not optional: the server holds the peer's connection open —
    /// revealing nothing — until this `request_id` is resolved, so Decline is
    /// sent just as deliberately as Approve. A missing id means the prompt was
    /// resolved twice, which the server treats as a harmless no-op anyway.
    fn route_lan_approval_action(&mut self, request_id: Option<u64>, action: LanApprovalAction) {
        let Some(request_id) = request_id else {
            tracing::warn!("LAN approval answered with no pending request id");
            return;
        };
        let approve = matches!(action, LanApprovalAction::Approve);
        tracing::info!(request_id, approve, "answering the LAN device-approval prompt");
        if let Err(error) = self.sink.lan_approval_decision(request_id, approve) {
            tracing::warn!(%error, "LAN approval decision dropped: IPC writer closed");
        }
    }

    /// Raise the close confirmation for a window-close request.
    ///
    /// Returns whether the platform may destroy the window: always `false`,
    /// because the server owns this window's sessions and has to be told what
    /// to do with them. The WM's close button, the quit chord, and a palette
    /// row all land here, so there is exactly one place a close is decided.
    fn request_window_close(&mut self, cx: &mut Context<Self>) -> bool {
        if self.dialog.is_some() {
            return false;
        }
        let session_count = self.shared.tabs.lock().map_or(0, |tabs| tabs.tabs().len());
        self.open_dialog(AnyDialog::Close(CloseDialog::new(session_count)), cx);
        false
    }

    /// Act on the close dialog's answer.
    ///
    /// "Quit Scribe" and "Kill Window" each put one frame on the wire and then
    /// wait: the window only goes away when the server acknowledges, so a
    /// server that never answers leaves a usable window rather than a
    /// half-closed one. Cancel — which Escape and a backdrop click also resolve
    /// to — does nothing at all.
    fn route_close_action(&mut self, action: CloseAction) {
        match action {
            CloseAction::QuitAll => self.request_quit_all(),
            CloseAction::CloseWindow => self.request_close_window(),
            CloseAction::Cancel => {}
        }
    }

    /// Ask the server to bring every window down, and wait for `QuitRequested`.
    fn request_quit_all(&self) {
        let Ok(mut lifecycle) = self.shared.lifecycle.lock() else {
            tracing::warn!("quit all dropped: window lifecycle mutex poisoned");
            return;
        };
        if !lifecycle.begin_quit_all() {
            tracing::debug!("quit all ignored: a shutdown is already in flight");
            return;
        }
        drop(lifecycle);
        tracing::info!("quit all — awaiting server acknowledgment");
        if let Err(error) = self.sink.quit_all() {
            tracing::warn!(%error, "quit all dropped: IPC writer closed");
            self.abandon_shutdown();
        }
    }

    /// Ask the server to destroy this window and its sessions, then wait for
    /// the matching `WindowClosed`.
    fn request_close_window(&self) {
        let Ok(mut lifecycle) = self.shared.lifecycle.lock() else {
            tracing::warn!("close window dropped: window lifecycle mutex poisoned");
            return;
        };
        let Some(window_id) = lifecycle.begin_close_window() else {
            tracing::warn!(
                "close window ignored: no window id from Welcome yet, or a shutdown is in flight"
            );
            return;
        };
        drop(lifecycle);
        tracing::info!(%window_id, "closing window permanently — awaiting server acknowledgment");
        if let Err(error) = self.sink.close_window(window_id) {
            tracing::warn!(%error, "close window dropped: IPC writer closed");
            self.abandon_shutdown();
        }
    }

    /// Release the shutdown slot after a request that never reached the wire.
    fn abandon_shutdown(&self) {
        if let Ok(mut lifecycle) = self.shared.lifecycle.lock() {
            lifecycle.abandon_shutdown();
        }
    }

    /// Report the window's current focus to the server, if it moved.
    ///
    /// Both producers converge here — the activation observer for an OS focus
    /// change, the lifecycle tick for a pane change the reader caused — and
    /// [`WindowLifecycle::focus_change`] collapses them into one gained/lost
    /// pair, dropping the report entirely when nothing actually moved.
    fn report_focus(&mut self) {
        let session = self.shared.active_session.lock().ok().and_then(|guard| *guard);
        let Ok(mut lifecycle) = self.shared.lifecycle.lock() else {
            return;
        };
        let Some(FocusReport { gained, lost }) = lifecycle.focus_change(session) else {
            return;
        };
        drop(lifecycle);
        tracing::debug!(?gained, ?lost, "reporting focus change");
        if let Err(error) = self.sink.focus_changed(gained, lost) {
            tracing::warn!(%error, "focus report dropped: IPC writer closed");
        }
    }

    /// Run every terminal bell the IPC reader queued through the suppression
    /// gate, on the thread that owns the window.
    ///
    /// The gate's inputs are refreshed first because all three live outside it:
    /// the focused pane is the shared `active_session` the reader and the tab
    /// shortcuts both move, and an update in flight is the shared
    /// [`UpdateState`] — the winit client read `update_available.is_none()` at
    /// exactly this point. Refreshing them before the drain is what makes a
    /// queued bell judged against the focus state it is actually delivered
    /// under, so a bell that arrived while a background tab was selected is
    /// still suppressed if that tab is the foreground pane by the time it lands.
    fn poll_bells(&mut self, cx: &mut Context<Self>) {
        let Ok(mut queued) = self.shared.bells.lock() else {
            tracing::warn!("bell queue mutex poisoned; dropping queued bells");
            return;
        };
        if queued.is_empty() {
            return;
        }
        let bells = std::mem::take(&mut *queued);
        drop(queued);
        let focused = self.shared.active_session.lock().ok().and_then(|guard| *guard);
        let updating = self.shared.update.lock().is_ok_and(|update| update.version().is_some());
        self.bell.update(cx, |controller, ctx| {
            if let Some(session_id) = focused {
                controller.focus_session(session_id, ctx);
            }
            controller.set_update_in_progress(updating);
            for session_id in bells {
                controller.on_bell(session_id, ctx);
            }
        });
    }

    /// Perform a bell the gate let through: ask the OS to draw attention to this
    /// window.
    ///
    /// This is the winit client's `handle_bell_event` tail — it called
    /// `request_user_attention(Informational)` on exactly the same condition —
    /// lowered onto GPUI's platform equivalent. On X11 that sets the `WM_HINTS`
    /// urgency flag, which is why the attention request is observable from
    /// outside the process even where no audible bell exists.
    fn on_bell_signal(event: BellEvent, window: &mut Window) {
        let BellEvent::Signal { session_id } = event;
        window.request_attention();
        tracing::info!(%session_id, "terminal bell requested window attention");
    }

    /// Run one window-lifecycle tick on the GPUI thread.
    ///
    /// Four jobs, all of which need the foreground: the queued terminal bells
    /// are gated and signalled here (the attention request is a window call), a
    /// server-acknowledged exit can only be performed here, a focus transition
    /// the IPC reader caused (a reattach moves the focused pane with no UI event
    /// behind it) is reconciled here, and the window-list poll sends from the
    /// view's own sink.
    fn poll_window_lifecycle(&mut self, cx: &mut Context<Self>) {
        self.poll_bells(cx);
        let exit = self.shared.lifecycle.lock().ok().and_then(|mut l| l.take_exit());
        if let Some(reason) = exit {
            match reason {
                ExitReason::QuitRequested => {
                    tracing::info!("quit requested by server — exiting");
                }
                ExitReason::WindowClosed => {
                    tracing::info!("window close acknowledged by server — exiting");
                }
            }
            cx.quit();
            return;
        }
        self.report_focus();
        self.poll_window_list();
        self.poll_lan_approval(cx);
    }

    /// Re-poll the server's window list when it is due.
    ///
    /// Gated on `remote.enabled` exactly like the winit client: the reply only
    /// feeds the status bar's owning-machine remote-control summary, which is
    /// not rendered at all while remote control is off.
    fn poll_window_list(&mut self) {
        if !self.config.config().config.remote.enabled {
            return;
        }
        if self.last_window_list_poll.elapsed() < WINDOW_LIST_POLL_INTERVAL {
            return;
        }
        self.last_window_list_poll = Instant::now();
        if let Err(error) = self.sink.list_windows() {
            tracing::warn!(%error, "window list poll dropped: IPC writer closed");
        }
    }

    /// Ask the server for a new session in the active workspace.
    ///
    /// The tab appears once the server answers with `SessionCreated`, so the
    /// strip never shows a tab whose PTY failed to spawn.
    fn create_tab(&self, command: Option<Vec<String>>) {
        let Some(workspace_id) = self.shared.tabs.lock().ok().and_then(|t| t.active_workspace())
        else {
            tracing::warn!("new tab ignored: no workspace is attached yet");
            return;
        };
        if let Err(error) =
            self.sink.create_session(workspace_id, self.focused_pane_size, None, command)
        {
            tracing::warn!(%error, "new tab dropped: IPC writer closed");
        }
    }

    /// Move the tab selection with `move_selection` and attach whatever it
    /// lands on. A `None` result means the selection did not move.
    ///
    /// A tab and a pane are different axes of the same window, so the switch
    /// resolves against the layout first: a session that is already on screen
    /// in some pane wins focus rather than being duplicated into the focused
    /// pane, and anything else takes over the pane the user is looking at.
    fn switch_tab(
        &mut self,
        move_selection: impl FnOnce(&mut TabSessions) -> Option<SessionId>,
        cx: &mut Context<Self>,
    ) {
        let Ok(mut tabs) = self.shared.tabs.lock() else { return };
        let Some(session_id) = move_selection(&mut tabs) else { return };
        drop(tabs);
        if let Some((workspace_id, pane)) = self.shell.pane_for_session(session_id, cx) {
            self.shell.focus_pane(workspace_id, pane, cx);
        } else if let Some(pane) = self.shell.focused_pane(cx) {
            self.adopt_session(pane, session_id);
            self.publish_pane_sizes(cx);
        }
        self.attach(session_id);
        // A tab switch moves the focused pane, which the server relays to PTY
        // applications as a CSI focus event — reported here rather than on the
        // next tick so the switched-to pane learns about it immediately.
        self.report_focus();
        self.sync_tabs(cx);
    }

    /// Point the client at `session_id`: attach it, announce the client size,
    /// and subscribe so the switched-to tab streams and reports its own state.
    ///
    /// The three frames go out on the one ordered writer channel and the server
    /// dispatches them in that order, so the `Subscribe` always lands after the
    /// attach that authorises it. The attach itself answers with a
    /// `SessionReplay` that repaints the tab; the subscription is what makes the
    /// server re-check the pane's working directory, so the status bar follows
    /// the switch instead of keeping the previous tab's chrome.
    fn attach(&self, session_id: SessionId) {
        if let Ok(mut guard) = self.shared.active_session.lock() {
            *guard = Some(session_id);
        }
        if let Ok(mut attached) = self.shared.attached.lock() {
            attached.insert(session_id);
        }
        // A pane owns only its slice of the window, so the size announced here
        // is the one the layout published for this session, not the window's.
        let size = self.pane_sizes.get(&session_id).copied().unwrap_or(self.terminal_size);
        let result = self
            .sink
            .attach_sessions(vec![session_id], vec![size])
            .and_then(|()| self.sink.resize(session_id, size))
            .and_then(|()| self.sink.subscribe(vec![session_id]));
        if let Err(error) = result {
            tracing::warn!(%error, "tab switch dropped: IPC writer closed");
        }
    }

    /// Close the focused tab's session. The strip updates on `SessionExited`.
    fn close_active_tab(&self) {
        let Some(session_id) = self.shared.tabs.lock().ok().and_then(|t| t.active_session()) else {
            return;
        };
        if let Err(error) = self.sink.close_session(session_id) {
            tracing::warn!(%error, "close tab dropped: IPC writer closed");
            return;
        }
        // The strip is pixels only, and the chord that gets here was shadowed
        // by an overlay for the whole of the rebuild; log the request so a
        // scripted E2E can assert the chord reached this action at all.
        tracing::info!(session = %session_id, "closing the active tab");
    }

    /// Open a second top-level terminal window.
    ///
    /// GPUI is multi-window in one process (the settings window already proves
    /// the pattern), so this stays in-process rather than re-spawning the
    /// binary the way the winit client's `spawn_client_process` had to. The new
    /// window gets its own [`Shared`] and its own IPC connection from
    /// [`start_window_backend`], so it is a genuinely separate client: the
    /// server registers a fresh window for it and its tab strip, status line,
    /// and grid are independent of this one's.
    fn open_new_window(&mut self, cx: &mut Context<Self>) {
        let terminal_size = self.terminal_size;
        let (shared, sink) = start_window_backend(terminal_size);
        open_window(cx, &shared, &sink, terminal_size);
        tracing::info!("opened a new terminal window");
    }

    /// The grid area every pane rect is resolved against, in real pixels.
    ///
    /// The paint path positions each pane as a *fraction* of this rect, so the
    /// layout only ever needs proportions — but the per-pane `Resize` divides
    /// the rect by the live cell box, which makes the units matter: measured in
    /// the font's own cells, a zoom step would move both the numerator and the
    /// denominator and the server would be told the same `cols`x`rows` at every
    /// font size, leaving the freed pixels dead. The measured area is therefore
    /// the source of truth, and a zoom (or any window resize) re-lays the grid
    /// into the window it actually has.
    ///
    /// The fallback is the nominal [`COLUMNS`]x[`ROWS`] box at the current
    /// metrics, which is exactly the size [`startup_window_size`] opens the
    /// window at — it stands in only for the frames before the grid canvas has
    /// reported its bounds for the first time.
    fn pane_viewport(&self) -> Rect {
        if let Some(bounds) = self.grid_area.get() {
            let width = f32::from(bounds.size.width);
            let height = f32::from(bounds.size.height);
            if width > 0.0 && height > 0.0 {
                return Rect { x: 0.0, y: 0.0, width, height };
            }
        }
        Rect {
            x: 0.0,
            y: 0.0,
            width: self.font.cell_width() * f32::from(COLUMNS),
            height: self.font.line_height * f32::from(ROWS),
        }
    }

    /// Republish the pane geometry when the measured grid area changed.
    ///
    /// The area is reported by the paint pass, so it lands one frame after the
    /// change that caused it — a window resize, a chrome band appearing, or the
    /// very first frame. Publishing from here (rather than from the canvas
    /// closure, which runs mid-paint) keeps every `Resize` on the render path
    /// the rest of the pane geometry already goes out on.
    fn sync_grid_geometry(&mut self, cx: &mut Context<Self>) {
        let viewport = self.pane_viewport();
        let measured = (viewport.width, viewport.height);
        if self.published_grid_area == Some(measured) {
            return;
        }
        self.published_grid_area = Some(measured);
        self.publish_pane_sizes(cx);
    }

    /// Split the focused pane and ask the server for the session it will host.
    fn split_pane(&mut self, direction: SplitDirection, cx: &mut Context<Self>) {
        if self.shell.split_focused_pane(direction, cx).is_none() {
            tracing::warn!(?direction, "split ignored: the window has no focused pane");
            return;
        }
        tracing::info!(?direction, panes = self.shell.pane_count(cx), "split the focused pane");
        self.request_pane_session();
        self.after_layout_change(cx);
    }

    /// Close the focused pane, falling back to closing the tab when the window
    /// is down to a single pane in a single workspace region.
    fn close_pane(&mut self, cx: &mut Context<Self>) {
        match self.shell.close_focused_pane(cx) {
            ClosedPane::Removed(sessions) => {
                for session_id in &sessions {
                    self.close_pane_session(*session_id);
                }
                tracing::info!(
                    closed = sessions.len(),
                    panes = self.shell.pane_count(cx),
                    "closed the focused pane"
                );
                self.focus_pane_session(cx);
                self.after_layout_change(cx);
            }
            ClosedPane::LastPane => {
                tracing::info!("close pane fell through to closing the last tab");
                self.close_active_tab();
            }
        }
    }

    /// Stop streaming a closed pane's session and tell the server to end it.
    fn close_pane_session(&mut self, session_id: SessionId) {
        self.detach_session(session_id);
        if let Err(error) = self.sink.close_session(session_id) {
            tracing::warn!(%error, "close pane dropped: IPC writer closed");
        }
    }

    /// Cycle focus to the next pane of the focused region.
    fn focus_next_pane(&mut self, cx: &mut Context<Self>) {
        let Some(pane) = self.shell.focus_next_pane(cx) else {
            tracing::debug!("cycle pane ignored: the region has a single pane");
            return;
        };
        tracing::info!(pane = pane.raw(), "focused pane moved");
        self.focus_pane_session(cx);
    }

    /// Move pane focus spatially inside the focused region.
    fn focus_pane(&mut self, direction: FocusDirection, cx: &mut Context<Self>) {
        let viewport = self.pane_viewport();
        let Some(pane) = self.shell.focus_pane_in_direction(direction, viewport, cx) else {
            tracing::debug!(?direction, "pane focus ignored: no pane in that direction");
            return;
        };
        tracing::info!(?direction, pane = pane.raw(), "focused pane moved");
        self.focus_pane_session(cx);
    }

    /// Split the window into another workspace region and seed it with a
    /// session.
    ///
    /// The region itself is client-local: the server still owns exactly one
    /// workspace for this window, so the seeded session is created in that
    /// workspace and the region is a layout construct until
    /// `ClientMessage::CreateWorkspace` is wired (bead .66).
    fn split_workspace(&mut self, direction: SplitDirection, cx: &mut Context<Self>) {
        let accent = self.next_region_accent(cx);
        let Some(workspace_id) = self.shell.split_workspace(direction, accent, cx) else {
            tracing::warn!(?direction, "workspace split ignored: no focused region");
            return;
        };
        tracing::info!(
            ?direction,
            %workspace_id,
            regions = self.shell.region_count(cx),
            "split the window into a new workspace region"
        );
        self.request_pane_session();
        self.after_layout_change(cx);
    }

    /// Move focus to the neighbouring workspace region.
    fn focus_workspace(&mut self, direction: FocusDirection, cx: &mut Context<Self>) {
        let viewport = self.pane_viewport();
        let Some(workspace_id) = self.shell.focus_workspace_in_direction(direction, viewport, cx)
        else {
            tracing::debug!(?direction, "workspace focus ignored: no region in that direction");
            return;
        };
        tracing::info!(?direction, %workspace_id, "focused workspace moved");
        self.focus_pane_session(cx);
    }

    /// A saturated, theme-derived accent for the next workspace region, so two
    /// regions never share a focus-ring colour at a glance.
    fn next_region_accent(&self, cx: &App) -> [f32; 4] {
        // The six bright ANSI slots after bright-red are the theme's own
        // high-chroma hues; cycling them keeps the accent inside the palette.
        let index = 9 + self.shell.region_count(cx) % 6;
        self.config.config().theme.ansi_colors.get(index).copied().unwrap_or(self.chrome.accent)
    }

    /// Ask the server for a session to fill the pane that just appeared.
    ///
    /// The pane was queued by the split, so the reconcile pass hands it the
    /// session as soon as `SessionCreated` lands.
    fn request_pane_session(&self) {
        let Some(workspace_id) = self.shared.tabs.lock().ok().and_then(|t| t.active_workspace())
        else {
            tracing::warn!("pane session ignored: no workspace is attached yet");
            return;
        };
        if let Err(error) =
            self.sink.create_session(workspace_id, self.focused_pane_size, None, None)
        {
            tracing::warn!(%error, "pane session dropped: IPC writer closed");
        }
    }

    /// Point the client at whatever session the focused pane now holds.
    ///
    /// A pane focus change is a tab selection change too: the strip keeps
    /// naming the pane the user types into, the server is told which pane has
    /// focus, and the attach makes the switched-to pane stream.
    fn focus_pane_session(&mut self, cx: &mut Context<Self>) {
        let Some(session_id) = self.shell.focused_session(cx) else {
            cx.notify();
            return;
        };
        if let Ok(mut tabs) = self.shared.tabs.lock() {
            let index = tabs.tabs().iter().position(|tab| tab.session_id == session_id);
            if let Some(index) = index {
                tabs.select(index);
            }
        }
        self.attach(session_id);
        self.report_focus();
        self.sync_tabs(cx);
        cx.notify();
    }

    /// Republish pane geometry after the layout moved, then repaint.
    fn after_layout_change(&mut self, cx: &mut Context<Self>) {
        self.publish_pane_sizes(cx);
        cx.notify();
    }

    /// Stop streaming `session_id` and drop the state its pane owned.
    fn detach_session(&mut self, session_id: SessionId) {
        self.pane_sizes.remove(&session_id);
        if let Ok(mut attached) = self.shared.attached.lock() {
            attached.remove(&session_id);
        }
        if let Ok(mut grids) = self.shared.panes.lock() {
            grids.forget(session_id);
        }
    }

    /// Tell the server (and each local grid) how big every pane now is.
    ///
    /// A `Resize` alone would leave the pane showing a grid it can no longer
    /// hold, because this client owns no PTY and never reflows locally, so each
    /// changed pane also reshapes its display grid and asks for the
    /// authoritative screen back. Unchanged panes are skipped, so a redraw
    /// storm never turns into a `RequestSnapshot` storm.
    fn publish_pane_sizes(&mut self, cx: &mut Context<Self>) {
        let viewport = self.pane_viewport();
        let cell_width = self.font.cell_width();
        let line_height = self.font.line_height;
        if cell_width <= 0.0 || line_height <= 0.0 {
            return;
        }
        let reported_width = round_positive_f32_to_u16(cell_width).max(1);
        let reported_height = round_positive_f32_to_u16(line_height).max(1);
        let placements = self.shell.placements(viewport, cx);
        let live: HashSet<SessionId> =
            placements.iter().filter_map(|placement| placement.session_id).collect();
        self.pane_sizes.retain(|session, _| live.contains(session));
        for placement in placements {
            let size = TerminalSize {
                cols: round_positive_f32_to_u16((placement.rect.width / cell_width).floor()).max(1),
                rows: round_positive_f32_to_u16((placement.rect.height / line_height).floor())
                    .max(1),
                cell_width: reported_width,
                cell_height: reported_height,
            };
            if placement.focused {
                // A new tab and a split both open into the focused pane, so the
                // size they announce has to be that pane's, not the window's.
                self.focused_pane_size = size;
            }
            let Some(session_id) = placement.session_id else { continue };
            if self.pane_sizes.get(&session_id) == Some(&size) {
                continue;
            }
            self.pane_sizes.insert(session_id, size);
            if let Ok(mut grids) = self.shared.panes.lock() {
                grids.resize(session_id, usize::from(size.cols), usize::from(size.rows));
            }
            let result = self
                .sink
                .resize(session_id, size)
                .and_then(|()| self.sink.request_snapshot(session_id));
            if let Err(error) = result {
                tracing::warn!(%error, "pane resize dropped: IPC writer closed");
            }
            tracing::info!(
                %session_id,
                %placement.workspace_id,
                pane = placement.pane_id.raw(),
                cols = size.cols,
                rows = size.rows,
                "published a pane's grid size"
            );
        }
    }

    /// Reconcile the pane layout with the sessions the server actually has.
    ///
    /// Runs once per frame because both halves of the truth move on their own
    /// threads: the reader owns the session list and the focused session, the
    /// GPUI thread owns the split trees. Three things can be out of step —
    /// the root region's workspace ID before the first `SessionList`, a pane
    /// whose session exited, and a freshly created session that has no pane
    /// yet — and each is settled here rather than from the reader, which must
    /// never touch GPUI entities.
    fn reconcile_panes(&mut self, cx: &mut Context<Self>) {
        let mut changed = false;
        if let Some(workspace_id) = self.shared.tabs.lock().ok().and_then(|t| t.active_workspace())
        {
            changed |= self.shell.adopt_server_workspace(workspace_id, cx);
        }
        let live: HashSet<SessionId> = self.shared.tabs.lock().map_or_else(
            |_| HashSet::new(),
            |tabs| tabs.tabs().iter().map(|tab| tab.session_id).collect(),
        );
        if !live.is_empty() {
            changed |= self.shell.retain_sessions(&live, cx);
        }
        let active = self.shared.active_session.lock().ok().and_then(|guard| *guard);
        if let Some(session_id) =
            active.filter(|session| !self.shell.shown_sessions().contains(session))
        {
            // A split queued the pane that asked for this session; anything
            // else (a new tab, a reattach, a refocus after an exit) belongs in
            // the pane the user is looking at.
            let target = self.shell.take_pending(cx).or_else(|| self.shell.focused_pane(cx));
            if let Some(pane) = target {
                self.adopt_session(pane, session_id);
                changed = true;
            }
        }
        if changed {
            self.publish_pane_sizes(cx);
        }
    }

    /// Show `session_id` in `pane`, streaming it from now on.
    fn adopt_session(&mut self, pane: PaneId, session_id: SessionId) {
        if let Some(displaced) = self.shell.assign_session(pane, session_id) {
            tracing::debug!(%displaced, pane = pane.raw(), "pane switched session");
        }
        if let Ok(mut attached) = self.shared.attached.lock() {
            attached.insert(session_id);
        }
        tracing::info!(%session_id, pane = pane.raw(), "pane adopted a session");
    }

    /// The last painted frame of `session_id`'s grid, or `None` when nothing
    /// has ever reached that pane.
    fn pane_content(&self, session_id: SessionId) -> Option<Content> {
        self.shared.panes.lock().ok()?.content(session_id)
    }

    /// Lower the pane layout onto absolutely positioned grid elements.
    ///
    /// Positions are fractions of the grid area, so the split ratios the pure
    /// tree computed survive any window size without the view having to measure
    /// device pixels. The focus ring is only drawn once a window actually has
    /// more than one pane, so an unsplit window paints exactly as before.
    ///
    /// Find matches, the split-scroll pin and the recorded grid bounds all
    /// belong to the focused pane: the overlay searched the pane the query was
    /// typed against, the pin follows that pane's viewport, and the bounds are
    /// what the mouse path hit-tests against. `focused` is therefore the
    /// snapshot [`Self::sync_split_scroll`] already pinned, and every other
    /// pane paints its own untouched grid.
    fn render_panes(&self, focused: Content, cx: &App) -> Vec<gpui::AnyElement> {
        let viewport = self.pane_viewport();
        if viewport.width <= 0.0 || viewport.height <= 0.0 {
            return Vec::new();
        }
        let placements = self.shell.placements(viewport, cx);
        let split = placements.len() > 1;
        let opacity = self.opacity;
        let background = surface(self.terminal_colors.background, opacity);
        let idle_border = surface(self.chrome.divider, opacity);
        let mut focused = Some(focused);
        placements
            .into_iter()
            .map(|placement| {
                let content = if placement.focused {
                    focused.take().unwrap_or_default()
                } else {
                    placement.session_id.and_then(|s| self.pane_content(s)).unwrap_or_default()
                };
                let colors = GridColors {
                    background,
                    cells: Arc::clone(&self.terminal_colors.cells),
                    opacity,
                };
                let mut pane = div()
                    .absolute()
                    .left(relative(placement.rect.x / viewport.width))
                    .top(relative(placement.rect.y / viewport.height))
                    .w(relative(placement.rect.width / viewport.width))
                    .h(relative(placement.rect.height / viewport.height))
                    .overflow_hidden();
                if split {
                    pane =
                        pane.border_1().border_color(pane_border(&placement, idle_border, opacity));
                }
                // Only the focused pane publishes its painted bounds: they are
                // what `cell_at` resolves a pointer against, and the mouse
                // path acts on the focused pane.
                let (highlights, bounds) = if placement.focused {
                    (self.find_highlights(&content, cx), Rc::clone(&self.grid_bounds))
                } else {
                    (Vec::new(), GridBounds::default())
                };
                pane.child(
                    TerminalElement::new(
                        content,
                        self.font.clone(),
                        colors,
                        self.highlight_colors,
                        bounds,
                    )
                    .with_highlights(highlights)
                    .paint(),
                )
                .into_any_element()
            })
            .collect()
    }

    /// Open the command palette, building its entry list from the live update /
    /// profile state, and subscribe to its confirm/dismiss events so a choice or
    /// an outside click tears the overlay down.
    fn open_command_palette(&mut self, cx: &mut Context<Self>) {
        self.context_menu = None;
        let profile_names = scribe_common::profiles::list_profiles().unwrap_or_default();
        let active = scribe_common::profiles::active_profile_name().ok();
        let entries = build_entries(None, &profile_names, active.as_deref());
        let colors = CommandPaletteColors::from(&self.chrome);
        let palette = cx.new(|cx| CommandPaletteView::new(colors, entries, cx));
        cx.subscribe(&palette, |this, _palette, event: &CommandPaletteEvent, ctx| {
            // Tear the overlay down first: a routed action may raise another
            // overlay (a dialog, the palette again), and clearing afterwards
            // would wipe it out.
            this.command_palette = None;
            ctx.notify();
            if let CommandPaletteEvent::Execute(action) = event {
                this.execute_palette_action(action.clone(), ctx);
            }
        })
        .detach();
        self.command_palette = Some(palette);
        cx.notify();
    }

    /// Open the find-in-scrollback overlay and subscribe to its query edits.
    ///
    /// The overlay owns no session and no sink, so every query edit comes back
    /// here as a [`FindOverlayEvent::QueryChanged`] and is lowered onto a real
    /// `SearchRequest` for the attached pane. It starts from the current
    /// [`FindResults`] version so a reply left over from a previous find is
    /// never adopted as an answer to the fresh, empty query.
    fn open_find_overlay(&mut self, cx: &mut Context<Self>) {
        self.context_menu = None;
        self.command_palette = None;
        let adopted = self.shared.find.lock().map_or(0, |results| results.version());
        let colors = FindOverlayColors::from(&self.chrome);
        let overlay = cx.new(|cx| FindOverlayView::new(colors, adopted, cx));
        cx.subscribe(&overlay, |this, _overlay, event: &FindOverlayEvent, ctx| match event {
            FindOverlayEvent::QueryChanged(query) => this.send_search_request(query),
            FindOverlayEvent::Dismissed => {
                this.find_overlay = None;
                ctx.notify();
            }
        })
        .detach();
        self.find_overlay = Some(overlay);
        tracing::info!("opened the find overlay");
        cx.notify();
    }

    /// Put one `SearchRequest` for `query` on the wire for the attached pane.
    ///
    /// An empty query sends nothing: the overlay has already dropped its
    /// matches, and asking the server to search for "" would only cost a round
    /// trip to be told there are no matches.
    fn send_search_request(&self, query: &str) {
        if query.is_empty() {
            return;
        }
        let Some(session_id) = self.shared.active_session.lock().ok().and_then(|guard| *guard)
        else {
            tracing::debug!("find query typed with no attached pane; nothing to search");
            return;
        };
        match self.sink.search_request(session_id, query.to_owned(), SEARCH_RESULT_LIMIT) {
            Ok(()) => tracing::info!(%session_id, %query, "sent search request"),
            Err(error) => tracing::warn!(%error, "search request dropped: IPC writer closed"),
        }
    }

    /// Fold the newest `SearchResults` reply into the open find overlay.
    ///
    /// Runs on every redraw because the reply arrives on the IPC reader thread,
    /// which cannot touch a GPUI entity; the reader bumps the repaint
    /// generation and this is where the result crosses into the view.
    fn sync_find_results(&mut self, cx: &mut Context<Self>) {
        let Some(overlay) = self.find_overlay.clone() else {
            return;
        };
        let Ok(results) = self.shared.find.lock() else {
            return;
        };
        overlay.update(cx, |view, ctx| view.adopt_results(&results, ctx));
    }

    /// The find-match spans to highlight on this frame's grid.
    fn find_highlights(
        &self,
        content: &Content,
        cx: &App,
    ) -> Vec<scribe_client_gpui::search::MatchHighlight> {
        let Some(overlay) = self.find_overlay.as_ref() else {
            return Vec::new();
        };
        let rows = content.rows.len();
        let cols = content.rows.first().map_or(0, Vec::len);
        overlay.read(cx).highlights(rows, cols)
    }

    /// Route a keystroke into the open find overlay.
    ///
    /// Ports the winit `handle_search_overlay_keyboard` table: Escape closes,
    /// Enter / Shift+Enter and the arrow keys cycle the highlighted match,
    /// Backspace and Delete edit the query, and any printable character extends
    /// it. Every key is consumed while the overlay is up — including the find
    /// chord itself, which must not reopen the overlay underneath itself.
    fn handle_find_overlay_key(
        overlay: &Entity<FindOverlayView>,
        event: &KeyDownEvent,
        cx: &mut Context<Self>,
    ) {
        let shift = event.keystroke.modifiers.shift;
        let claimed_by_modifier = event.keystroke.modifiers.control
            || event.keystroke.modifiers.alt
            || event.keystroke.modifiers.platform;
        match event.keystroke.key.as_str() {
            "escape" => overlay.update(cx, FindOverlayView::dismiss),
            "enter" if shift => overlay.update(cx, FindOverlayView::prev_match),
            "enter" | "down" => overlay.update(cx, FindOverlayView::next_match),
            "up" => overlay.update(cx, FindOverlayView::prev_match),
            "backspace" => overlay.update(cx, FindOverlayView::pop_char),
            "delete" => overlay.update(cx, FindOverlayView::clear_query),
            _ => {
                if claimed_by_modifier {
                    return;
                }
                if let Some(text) = event.keystroke.key_char.as_ref().filter(|t| !t.is_empty()) {
                    let text = text.clone();
                    overlay.update(cx, |view, ctx| view.push_str(&text, ctx));
                }
            }
        }
        // The match highlights are painted by *this* view, not by the overlay
        // entity, so cycling the current match has to dirty the grid too — a
        // notify on the overlay alone would move the `n/m` counter while the
        // accent stayed on the previous match until the next server frame.
        cx.notify();
    }

    /// Open the right-click context menu at `position` with a representative item
    /// set (selection + OSC 8 URL + one smart-selection row), subscribing so a
    /// choice runs through [`Self::dispatch_context_menu_action`] and a dismiss
    /// closes the overlay.
    ///
    /// The smart-selection row is a demo stand-in until the rule engine is on a
    /// live path: it is the menu's one entry whose effect lands in the attached
    /// pane, so the scripted E2E can assert that a clicked row actually reached
    /// the PTY rather than only that the overlay closed.
    fn open_context_menu(&mut self, position: Point<gpui::Pixels>, cx: &mut Context<Self>) {
        self.command_palette = None;
        let colors = ContextMenuColors::from(&self.chrome);
        // The demo row stays first so the fixed-offset row assertions in the
        // overlay E2E keep addressing the same row; the live smart-selection
        // rows for the cell under the pointer are appended after it.
        let mut smart_actions = vec![MenuItem {
            label: DEMO_SMART_ACTION_LABEL.to_owned(),
            action: ContextMenuAction::SendText(format!("{DEMO_SMART_ACTION_TEXT}\n")),
            enabled: true,
        }];
        smart_actions.extend(self.smart_selection_rows(position));
        let request = ContextMenuRequest {
            has_selection: true,
            osc8_uri: Some("https://example.com/spec".into()),
            smart_actions,
            ..Default::default()
        };
        let menu = cx.new(|cx| ContextMenuView::new(colors, request, position, cx));
        cx.subscribe(&menu, |this, _menu, event: &ContextMenuEvent, ctx| {
            this.context_menu = None;
            ctx.notify();
            if let ContextMenuEvent::Selected(action) = event {
                this.dispatch_context_menu_action(action.clone(), ctx);
            }
        })
        .detach();
        self.context_menu = Some(menu);
        cx.notify();
    }

    /// Push this frame's split-scroll gate into the terminal and take the
    /// resulting snapshot.
    ///
    /// The config toggle and the AI-provider check are shell state, but the
    /// scroll position and the alternate-screen flag are terminal state, so the
    /// decision is split across the boundary: the shell hands over what it
    /// knows and reads back how many rows the pin ended up taking.
    fn sync_split_scroll(&mut self) -> Content {
        let eligibility = self.split_scroll_eligibility();
        let content = self
            .with_focused_grid(|terminal| {
                terminal.set_split_scroll_eligibility(eligibility);
                terminal.content()
            })
            .unwrap_or_default();
        self.split_scroll.pin_height = self.font.line_height * pin_rows_f32(content.pin_rows);
        content
    }

    /// Whether config and the focused session's AI provider permit a split.
    fn split_scroll_eligibility(&self) -> SplitScrollEligibility {
        let terminal_config = &self.config.config().config.terminal;
        let scroll_pin_enabled = terminal_config.scroll.scroll_pin;
        let ai_provider_enabled = self
            .shared
            .active_session
            .lock()
            .ok()
            .and_then(|guard| *guard)
            .and_then(|session_id| {
                let ai = self.shared.ai.lock().ok()?;
                ai.tracker.provider_for_session(session_id)
            })
            .is_some_and(|provider| terminal_config.ai_provider_enabled(provider));
        SplitScrollEligibility { scroll_pin_enabled, ai_provider_enabled }
    }

    /// Route a left click on the terminal grid.
    ///
    /// The only click target the grid owns today is the split-scroll jump chip,
    /// which is painted inside the canvas and so cannot be a GPUI child
    /// element; everything else falls through untouched.
    fn click_grid(&mut self, position: Point<gpui::Pixels>, cx: &mut Context<Self>) {
        if self.split_scroll.pin_height <= 0.0 {
            return;
        }
        let Some(bounds) = self.grid_bounds.get() else {
            return;
        };
        let (rows, pin_rows) = self
            .with_focused_grid(|terminal| (terminal.content().rows.len(), terminal.pin_rows()))
            .unwrap_or((0, 0));
        if !hits_jump_chip(bounds, &self.font, rows, pin_rows, position) {
            return;
        }
        tracing::info!("split-scroll jump chip clicked");
        self.scroll_terminal(Scroll::Bottom, cx);
    }

    /// The live smart-selection rows for the grid cell under `position`.
    ///
    /// Empty whenever the pointer is off the grid, no rule matched, or the
    /// matched rules carry no actions — which is the ordinary case over blank
    /// space, so an ordinary right-click still gets the plain menu.
    fn smart_selection_rows(&self, position: Point<gpui::Pixels>) -> Vec<MenuItem> {
        let Some(bounds) = self.grid_bounds.get() else {
            return Vec::new();
        };
        let Some(cell) = cell_at(bounds, &self.font, position) else {
            return Vec::new();
        };
        let Some(candidates) = self.with_focused_grid(|terminal| {
            terminal.smart_selection_actions(&self.smart_selection, cell)
        }) else {
            return Vec::new();
        };
        let expansion = self.action_expansion_values();
        let context = ActionExpansionContext {
            cwd: expansion.cwd.as_deref(),
            user: &expansion.user,
            host: &expansion.host,
        };
        candidates
            .iter()
            .inspect(|candidate: &&SmartSelectionCandidate| {
                tracing::info!(
                    rule = %candidate.rule_name,
                    text = %candidate.text,
                    row = cell.row,
                    col = cell.col,
                    "smart selection matched",
                );
            })
            .flat_map(|candidate| candidate.resolved_actions(&context))
            .map(smart_selection_menu_item)
            .collect()
    }

    /// The `\(path)` / `\(user)` / `\(host)` values a smart-selection action
    /// parameter interpolates against, read from the server-reported chrome.
    fn action_expansion_values(&self) -> ActionExpansionValues {
        let cwd = self.shared.active_session.lock().ok().and_then(|guard| *guard).and_then(
            |session_id| {
                let metadata = self.shared.chrome_metadata.lock().ok()?;
                let cwd = metadata.session(session_id)?.cwd.as_ref()?;
                Some(cwd.to_string_lossy().into_owned())
            },
        );
        ActionExpansionValues {
            cwd,
            user: std::env::var("USER").unwrap_or_default(),
            host: read_hostname(),
        }
    }

    /// Open a modal dialog, subscribing so a choice or a backdrop click tears the
    /// overlay down. The other overlays are dismissed so only one modal is up.
    ///
    /// The resolved [`DialogOutcome`] is routed before the overlay is dropped,
    /// so a modal that owns a side effect — today the update confirmation —
    /// performs it. Modals whose outcome is not wired yet simply close.
    fn open_dialog(&mut self, dialog: AnyDialog, cx: &mut Context<Self>) {
        self.command_palette = None;
        self.context_menu = None;
        let colors = DialogColors::from(&self.chrome);
        let view = cx.new(|cx| DialogView::new(dialog, colors, cx));
        cx.subscribe(&view, |this, _view, event: &DialogEvent, ctx| {
            let DialogEvent::Chosen(outcome) = event;
            // Tear the overlay down first: a routed action may raise another
            // overlay, and clearing afterwards would wipe it out.
            this.dialog = None;
            ctx.notify();
            this.route_dialog_outcome(*outcome);
        })
        .detach();
        self.dialog = Some(view);
        cx.notify();
    }

    /// Open the update confirmation the centred status-bar CTA resolves to.
    ///
    /// Nothing opens when the CTA is purely informational ("Downloading...",
    /// "Update failed") because [`UpdateState::confirmation`] has no dialog for
    /// those states.
    fn open_update_dialog(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.shared.update.lock().ok().and_then(|state| state.confirmation())
        else {
            return;
        };
        let kind = match &dialog {
            AnyDialog::Update(update) => Some(update.kind()),
            _ => None,
        };
        self.open_dialog(dialog, cx);
        self.update_dialog_kind = kind;
    }

    /// Route the update confirmation's choice onto the wire.
    ///
    /// Confirming an available install sends `TriggerUpdate` and clears the
    /// pending version so the CTA stops offering it; declining sends
    /// `DismissUpdate` so the server stops re-notifying about this version.
    /// The restart-required flow only closes: its "Continue" spawns the
    /// platform cold-restart helper, which the GPUI shell does not host yet, so
    /// it is logged rather than silently swallowed.
    fn route_update_action(&mut self, action: UpdateAction) {
        let kind = self.update_dialog_kind;
        match (kind, action) {
            (Some(UpdateDialogKind::InstallAvailable), UpdateAction::Primary) => {
                self.mutate_update_state(UpdateState::on_triggered);
                if let Err(error) = self.sink.trigger_update() {
                    tracing::warn!(%error, "dropped TriggerUpdate: IPC writer closed");
                }
            }
            (Some(UpdateDialogKind::InstallAvailable), UpdateAction::Secondary) => {
                self.mutate_update_state(UpdateState::on_dismissed);
                if let Err(error) = self.sink.dismiss_update() {
                    tracing::warn!(%error, "dropped DismissUpdate: IPC writer closed");
                }
            }
            (Some(UpdateDialogKind::RestartRequired), UpdateAction::Primary) => {
                tracing::warn!("deferred cold restart helper is not wired in the GPUI shell");
            }
            (Some(UpdateDialogKind::RestartRequired), UpdateAction::Secondary) => {
                tracing::info!("user postponed the deferred cold restart");
            }
            (None, _) => tracing::warn!("update action without an open update dialog"),
        }
    }

    /// Apply `mutate` to the shared update state from the GPUI thread.
    fn mutate_update_state(&self, mutate: impl FnOnce(&mut UpdateState)) {
        let Ok(mut guard) = self.shared.update.lock() else {
            tracing::warn!("update state mutex poisoned; dropping update decision");
            return;
        };
        mutate(&mut guard);
    }

    /// Open the workspace-notes modal for a demo workspace, request its
    /// authoritative notes over the frozen IPC protocol, seed a representative
    /// note set, and subscribe so the shell routes each control — Save/archive
    /// become `WorkspaceNotesMutate`, Close tears the overlay down. Proves the
    /// modal surface plus the `WorkspaceNotesGet`/`WorkspaceNotesMutate` sink
    /// path end to end for the visual E2E, mirroring the other overlay demos.
    fn open_workspace_notes_modal(&mut self, cx: &mut Context<Self>) {
        self.command_palette = None;
        self.context_menu = None;
        let workspace_id = WorkspaceId::new();
        let colors = WorkspaceNotesModalColors::from(&self.chrome);
        let notes = demo_workspace_notes(workspace_id);
        let modal = cx.new(|cx| {
            let mut view = WorkspaceNotesModalView::new(&colors, cx);
            view.open(workspace_id, String::new(), cx);
            view.set_notes(notes, Vec::new(), cx);
            view
        });
        // Request the server's authoritative copy; the reply hydrates the modal
        // through `set_notes` once the shell's inbound wiring lands.
        if let Err(error) = self.sink.workspace_notes_get(vec![workspace_id]) {
            tracing::warn!(%error, "workspace notes get dropped: IPC writer closed");
        }
        cx.subscribe(&modal, |this, modal, action, ctx| {
            this.route_workspace_notes_action(&modal, action, ctx);
        })
        .detach();
        self.workspace_notes_modal = Some(modal);
        cx.notify();
    }

    /// Route one modal control to its side effect: state transitions update the
    /// modal, Save/archive emit a `WorkspaceNotesMutate`, and Close clears it.
    fn route_workspace_notes_action(
        &mut self,
        modal: &Entity<WorkspaceNotesModalView>,
        action: &WorkspaceNotesModalAction,
        cx: &mut Context<Self>,
    ) {
        match action {
            WorkspaceNotesModalAction::Close => {
                self.workspace_notes_modal = None;
                cx.notify();
            }
            WorkspaceNotesModalAction::Save => {
                if let Some(mutation) = modal.read(cx).save_mutation() {
                    self.send_workspace_notes_mutation(mutation);
                }
            }
            WorkspaceNotesModalAction::CancelEdit => {
                modal.update(cx, |m, ctx| {
                    m.cancel_edit();
                    ctx.notify();
                });
            }
            WorkspaceNotesModalAction::ShowActive => {
                modal.update(cx, |m, ctx| {
                    m.set_view(
                        scribe_client_gpui::workspace_notes_modal::WorkspaceNotesView::Active,
                        ctx,
                    );
                });
            }
            WorkspaceNotesModalAction::ShowArchive => {
                modal.update(cx, |m, ctx| {
                    m.set_view(
                        scribe_client_gpui::workspace_notes_modal::WorkspaceNotesView::Archive,
                        ctx,
                    );
                });
            }
            WorkspaceNotesModalAction::EditActive(note_id) => {
                let note_id = note_id.clone();
                modal.update(cx, |m, ctx| m.begin_active_edit_by_id(&note_id, ctx));
            }
            WorkspaceNotesModalAction::EditArchived(note_id) => {
                let note_id = note_id.clone();
                modal.update(cx, |m, ctx| m.begin_archived_edit_by_id(&note_id, ctx));
            }
            WorkspaceNotesModalAction::EditAllArchive => {
                modal.update(cx, WorkspaceNotesModalView::begin_archive_bulk_edit_all);
            }
            WorkspaceNotesModalAction::ArchiveDone(note_id) => {
                if let Some(mutation) =
                    modal.read(cx).archive_mutation(note_id, ArchiveReason::Done)
                {
                    self.send_workspace_notes_mutation(mutation);
                }
            }
            WorkspaceNotesModalAction::ArchiveRemoved(note_id) => {
                if let Some(mutation) =
                    modal.read(cx).archive_mutation(note_id, ArchiveReason::Removed)
                {
                    self.send_workspace_notes_mutation(mutation);
                }
            }
        }
    }

    /// Enqueue a workspace-notes mutation on the outbound sink.
    fn send_workspace_notes_mutation(
        &self,
        mutation: scribe_common::protocol::WorkspaceNotesMutation,
    ) {
        if let Err(error) = self.sink.workspace_notes_mutate(mutation) {
            tracing::warn!(%error, "workspace notes mutation dropped: IPC writer closed");
        }
    }

    /// Apply one keystroke to the open workspace-notes modal: Escape/Enter emit
    /// Close/Save, Backspace deletes, and printable input types into the buffer.
    fn handle_notes_modal_key(
        modal: &Entity<WorkspaceNotesModalView>,
        event: &KeyDownEvent,
        cx: &mut Context<Self>,
    ) {
        let typed = event.keystroke.key_char.as_ref().filter(|t| !t.is_empty()).cloned();
        match event.keystroke.key.as_str() {
            "escape" => modal.update(cx, |_, ctx| ctx.emit(WorkspaceNotesModalAction::Close)),
            "enter" => modal.update(cx, |_, ctx| ctx.emit(WorkspaceNotesModalAction::Save)),
            "backspace" => modal.update(cx, WorkspaceNotesModalView::pop_char),
            _ => {
                if let Some(text) = typed {
                    modal.update(cx, |m, ctx| text.chars().for_each(|ch| m.push_char(ch, ctx)));
                }
            }
        }
    }

    /// Claim a keystroke for a shell-owned overlay, with no overlay up yet.
    ///
    /// Two things are claimed ahead of the PTY here: the configured
    /// command-palette chord, and the fixed [`OverlayChord`] table. Both
    /// decisions are read off the live bindings before anything is opened, so
    /// a saved keybinding edit takes effect on the very next keystroke.
    ///
    /// Returns `false` for everything else — including a chord
    /// [`translate_overlay_chord`] declined because a configured binding claims
    /// it — so the keystroke goes on to [`Self::handle_binding`].
    fn claim_shell_chord(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        let Some(input) = KeyInput::from_key_down(event) else {
            return false;
        };
        let bindings = self.config.bindings();
        let opens_palette =
            matches!(translate_key_action(&input, bindings), Some(KeyAction::OpenCommandPalette));
        let chord = translate_overlay_chord(&input, bindings);

        if opens_palette {
            self.open_command_palette(cx);
            return true;
        }
        let Some(chord) = chord else {
            return false;
        };
        self.open_overlay_chord(chord, cx);
        true
    }

    /// Open the surface a shell-owned [`OverlayChord`] names.
    ///
    /// Split out from the key path so the chord table and the surfaces it opens
    /// stay in one place; the chord-versus-binding precedence itself lives in
    /// [`translate_overlay_chord`] and is unit-tested there.
    fn open_overlay_chord(&mut self, chord: OverlayChord, cx: &mut Context<Self>) {
        match chord {
            OverlayChord::TooltipDemo => {
                self.tooltip_demo = !self.tooltip_demo;
                cx.notify();
            }
            OverlayChord::CloseDialog => {
                self.request_window_close(cx);
            }
            OverlayChord::ClipboardDialog => {
                self.open_dialog(
                    AnyDialog::Clipboard(ClipboardDialog::new(
                        scribe_common::protocol::PromptId(1),
                        scribe_common::protocol::ClipboardOp::Write,
                        scribe_common::protocol::ClipboardSelection::Clipboard,
                        Some("export TOKEN=hunter2".to_owned()),
                    )),
                    cx,
                );
            }
            OverlayChord::WorkspaceNotes => self.open_workspace_notes_modal(cx),
            OverlayChord::ViMode => self.toggle_vi_mode(cx),
        }
    }

    /// Enter or leave vi / copy mode over the terminal grid.
    fn toggle_vi_mode(&mut self, cx: &mut Context<Self>) {
        let Some(active) = self.with_focused_grid(|terminal| {
            terminal.toggle_vi_mode();
            terminal.is_vi_mode()
        }) else {
            return;
        };
        tracing::info!(active, "vi mode toggled");
        cx.notify();
    }

    /// Route a keystroke while vi / copy mode owns the keyboard.
    ///
    /// Vi mode is a *mode*, not an overlay: it sits between the shell-owned
    /// chords and the configured bindings so a bound chord (a new tab, a zoom
    /// step) still works while navigating, but a bare motion key moves the vi
    /// cursor instead of reaching the PTY. Any other bare key is swallowed —
    /// leaking `j` into the shell while the user is reading scrollback is
    /// exactly the failure copy mode exists to prevent.
    fn handle_vi_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        if self.with_focused_grid(|terminal| terminal.is_vi_mode()) != Some(true) {
            return false;
        }
        let modifiers = event.keystroke.modifiers;
        if modifiers.control || modifiers.alt || modifiers.platform {
            return false;
        }
        // A configured binding always wins, the same precedence
        // `translate_overlay_chord` applies: `shift+pageup` has to keep paging
        // the scrollback while the vi cursor is up.
        let yields_to_binding = KeyInput::from_key_down(event)
            .is_some_and(|input| translate_key_action(&input, self.config.bindings()).is_some());
        if yields_to_binding {
            return false;
        }
        let key = event.keystroke.key.as_str();
        if matches!(key, "escape" | "q") {
            self.toggle_vi_mode(cx);
            return true;
        }
        if let Some(motion) = vi_motion_for_key(key, modifiers.shift) {
            self.with_focused_grid(|terminal| terminal.vi_motion(motion));
            cx.notify();
        }
        true
    }

    /// Route a keystroke while an overlay owns the keyboard. Returns `true` when
    /// the key was consumed by an overlay (and must not reach the PTY).
    fn handle_overlay_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        let mods = &event.keystroke.modifiers;
        // Feature 015 (T020): a pending control request is a full-window modal —
        // the holder (or the owner while control is unheld) answers it before
        // anything else reaches a binding, an overlay, or the PTY.
        if self.share_prompt_pending() && self.run_share_key(event, cx) {
            return true;
        }
        let overlay_free = self.dialog.is_none()
            && self.workspace_notes_modal.is_none()
            && self.find_overlay.is_none();

        // Config-driven shortcuts are matched against the live bindings, which
        // the config watcher re-parses on every reload — so a saved keybinding
        // edit takes effect on the very next keystroke, with no restart. Only
        // the palette intercept and the shell-owned overlay chords are claimed
        // here; every other translation falls through to `handle_binding` and
        // the terminal encoder below.
        //
        // The overlay chords are resolved by `translate_overlay_chord`, which
        // yields to any configured binding. That precedence is load-bearing: a
        // chord claimed here never reaches `handle_binding`, so without it a
        // hard-coded overlay chord silently shadows a bound action (the
        // `close_tab` / `ctrl+shift+q` collision).
        if overlay_free && self.claim_shell_chord(event, cx) {
            return true;
        }

        // A modal dialog owns the keyboard while it is up: Tab/Shift+Tab cycle
        // focus, Enter activates the focused button, Esc dismisses (safe action).
        if let Some(dialog) = self.dialog.clone() {
            match event.keystroke.key.as_str() {
                "escape" => dialog.update(cx, DialogView::dismiss),
                "enter" => dialog.update(cx, DialogView::confirm),
                "tab" if mods.shift => dialog.update(cx, DialogView::focus_prev),
                "tab" | "right" => dialog.update(cx, DialogView::focus_next),
                "left" => dialog.update(cx, DialogView::focus_prev),
                _ => {}
            }
            return true;
        }

        if let Some(modal) = self.workspace_notes_modal.clone() {
            Self::handle_notes_modal_key(&modal, event, cx);
            return true;
        }

        // The find overlay is a text field over the grid: while it is up every
        // keystroke belongs to it, so nothing here can leak to the PTY or
        // reopen the overlay through its own chord.
        if let Some(overlay) = self.find_overlay.clone() {
            Self::handle_find_overlay_key(&overlay, event, cx);
            return true;
        }

        let Some(palette) = self.command_palette.clone() else {
            return false;
        };
        let key = event.keystroke.key.as_str();
        match key {
            "escape" => palette.update(cx, CommandPaletteView::dismiss),
            "enter" => palette.update(cx, CommandPaletteView::confirm),
            "backspace" => palette.update(cx, CommandPaletteView::pop_char),
            "up" => palette.update(cx, CommandPaletteView::prev_item),
            "down" => palette.update(cx, CommandPaletteView::next_item),
            _ => {
                if let Some(text) = event.keystroke.key_char.as_ref().filter(|t| !t.is_empty()) {
                    let text = text.clone();
                    palette.update(cx, |p, ctx| p.push_str(&text, ctx));
                }
            }
        }
        true
    }

    /// Run a keystroke through the configured bindings before the PTY encoder.
    ///
    /// This is the level 1–3 intercept the legacy client ran ahead of its byte
    /// encoder: a bound layout/tab shortcut is executed here and never reaches
    /// the terminal, a bound palette shortcut opens the overlay, and a bound
    /// terminal shortcut sends its fixed escape sequence. Returns `true` when
    /// the key was consumed, leaving level 4 ([`Self::on_key_down`]) for
    /// everything else.
    fn handle_binding(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        let Some(input) = KeyInput::from_key_down(event) else {
            return false;
        };
        let Some(action) = translate_key_action(&input, self.config.bindings()) else {
            return false;
        };
        // One dispatcher for chords and palette rows alike; the palette chord
        // is normally claimed earlier by `handle_overlay_key`, and reaching it
        // here simply opens the overlay.
        self.dispatch_key_action(action, cx);
        true
    }

    /// Handle a window activation change from the platform.
    ///
    /// This is the client's focus observer: the server relays CSI focus events
    /// (`\x1b[I` / `\x1b[O`) to PTY applications that enabled DECSET 1004, so
    /// both edges are reported — a blurred window focuses no pane.
    ///
    /// Compositor overlays never send focus events, so a genuine activation
    /// also means the user really is back: drop the guard's reactivation
    /// debounce rather than eating the keystroke that follows a real refocus.
    fn on_activation(&mut self, window: &Window, cx: &mut Context<Self>) {
        let active = window.is_window_active();
        if let Ok(mut lifecycle) = self.shared.lifecycle.lock() {
            lifecycle.set_window_active(active);
        }
        // A blurred window makes every pane a background pane as far as the bell
        // gate is concerned, which is the half of the winit condition that has
        // nothing to do with which tab is selected.
        self.bell.update(cx, |controller, _| controller.set_window_focused(active));
        self.report_focus();
        if !active {
            return;
        }
        if let Some(guard) = self.x11_focus.as_mut() {
            guard.clear_reactivation_debounce();
        }
    }

    /// Refresh the X11 active-window guard's cached state (no-op off X11).
    ///
    /// Driven by [`drive_x11_focus_polls`] so an overlay that opens and closes
    /// between keystrokes still arms the reactivation debounce.
    fn poll_x11_focus(&mut self, _cx: &mut Context<Self>) {
        if let Some(guard) = self.x11_focus.as_mut() {
            guard.poll();
        }
    }

    /// Returns `true` when a compositor overlay (e.g. a screenshot tool) is
    /// covering the window, so `event` must not reach any keyboard consumer.
    ///
    /// This is the first gate on the key path — ahead of overlays, bindings,
    /// and the PTY encoder — because a keystroke the user aimed at the overlay
    /// (Enter to confirm a screenshot) must not land anywhere in the client.
    fn compositor_overlay_active(&mut self, event: &KeyDownEvent) -> bool {
        let Some(guard) = self.x11_focus.as_mut() else {
            return false;
        };
        if !guard.should_suppress_key() {
            return false;
        }
        // Input vanishing is exactly the symptom this guard produces, so say so
        // once per dropped keystroke rather than leaving it silent.
        tracing::info!(key = %event.keystroke.key, "x11 focus guard suppressed keystroke");
        true
    }

    /// Encodes a keystroke and enqueues it as `KeyInput` for the attached pane.
    ///
    /// Interim passthrough encoder: printable characters plus a handful of
    /// control keys. The full kitty/CSI-u encoder lands with the input-encoder
    /// port; this only proves the outbound [`IpcSink`] path end to end.
    ///
    /// A live share viewer never reaches the encoder: its keystroke is consumed
    /// by [`Self::run_share_key`], which raises the take-control affordance
    /// instead of leaking a keystroke the server would drop anyway.
    fn on_key_down(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        if self.run_share_key(event, cx) {
            return;
        }
        let Some(bytes) = encode_key(event) else {
            return;
        };
        self.send_key_bytes(bytes);
    }

    /// Whether a `ControlRequested` grant/deny prompt is currently modal.
    fn share_prompt_pending(&self) -> bool {
        self.shared.share.lock().is_ok_and(|share| share.has_prompt())
    }

    /// Run a keystroke through the feature-015 share surfaces, returning `true`
    /// when they consumed it.
    ///
    /// The decision table itself lives in
    /// [`ShareChrome::intercept_key`](scribe_client_gpui::share::ShareChrome::intercept_key);
    /// this is the shell half that lowers the GPUI key, sends any resulting
    /// `ControlClaim` / `ControlGrant` through the [`IpcSink`], and repaints.
    fn run_share_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        let key = match event.keystroke.key.as_str() {
            "enter" => ShareKey::Enter,
            "escape" => ShareKey::Escape,
            _ => ShareKey::Other,
        };
        let Ok(outcome) = self.shared.share.lock().map(|mut share| share.intercept_key(key)) else {
            return false;
        };
        match outcome {
            ShareKeyOutcome::Passthrough => return false,
            // Logged, not silent: a swallowed keystroke is invisible on the wire
            // (nothing is sent) and near-invisible on screen, so this line is
            // what the app-level test asserts suppression from.
            ShareKeyOutcome::Suppressed => {
                tracing::info!(key = %event.keystroke.key, "share surfaces swallowed a keystroke");
            }
            ShareKeyOutcome::Emit(intent) => {
                if let Err(error) = self.sink.control_intent(intent) {
                    tracing::warn!(%error, "dropped control intent: IPC writer closed");
                } else {
                    tracing::info!(?intent, "sent share control intent");
                }
            }
        }
        cx.notify();
        true
    }

    /// Clear an expired control hint so a stale banner never lingers on a window
    /// that has stopped repainting for any other reason.
    fn expire_share_hint(&mut self, cx: &mut Context<Self>) {
        if self.shared.share.lock().is_ok_and(|mut share| share.expire_hint()) {
            cx.notify();
        }
    }

    /// Snap a scrolled viewport back to the live bottom before a keystroke.
    ///
    /// Typing into a pane you are reading scrollback in should show you what
    /// you typed, so the ordinary case jumps to the bottom. The exception is
    /// the split-scroll pin: it already shows the live prompt, so it survives
    /// every keystroke except `Enter`, which submits and therefore ends the
    /// "compose while reading" session. Ported one-for-one from the winit
    /// client's `send_key_bytes` seam.
    fn snap_to_bottom_for_input(&self, bytes: &[u8]) {
        let snapped = self.with_focused_grid(|terminal| {
            if terminal.display_offset() == 0 {
                return None;
            }
            let pinned = terminal.pin_rows() > 0;
            if pinned && bytes != b"\r" {
                return None;
            }
            terminal.set_split_scroll_eligibility(SplitScrollEligibility::default());
            terminal.scroll(Scroll::Bottom);
            Some(pinned)
        });
        if let Some(Some(pinned)) = snapped {
            tracing::info!(pinned, "snapped the viewport to the live bottom for input");
        }
    }

    /// Enqueue already-encoded bytes for the attached pane.
    fn send_key_bytes(&self, bytes: Vec<u8>) {
        let session_id = self.shared.active_session.lock().ok().and_then(|guard| *guard);
        let Some(session_id) = session_id else {
            return;
        };
        self.snap_to_bottom_for_input(&bytes);
        if let Err(error) = self.sink.key_input(session_id, bytes, true) {
            tracing::warn!(%error, "dropped keystroke: IPC writer closed");
            return;
        }
        // Perf gate: start the echo round-trip clock the PTY-output path stops.
        scribe_common::perf_probe::record_input_sent();
    }

    /// Publish this frame to the perf rig's probe, along with the tab sessions
    /// and focused session the rig uses as its "only type into a pane I opened"
    /// interlock. Both are no-ops outside a rig run.
    fn report_perf_frame(&self) {
        scribe_common::perf_probe::record_frame();
        if !scribe_common::perf_probe::is_active() {
            return;
        }
        let sessions = self
            .shared
            .tabs
            .lock()
            .map(|tabs| tabs.tabs().iter().map(|entry| entry.session_id).collect())
            .unwrap_or_default();
        let focused = self.shared.active_session.lock().ok().and_then(|guard| *guard);
        scribe_common::perf_probe::record_sessions(sessions, focused);
    }
}

/// Owned backing store for an [`ActionExpansionContext`], which borrows every
/// field it interpolates.
struct ActionExpansionValues {
    cwd: Option<String>,
    user: String,
    host: String,
}

/// Widen a pin-row count for pixel arithmetic without a lossy cast.
fn pin_rows_f32(rows: usize) -> f32 {
    f32::from(u16::try_from(rows).unwrap_or(u16::MAX))
}

/// This machine's hostname, for `\(host)` in a smart-selection parameter.
fn read_hostname() -> String {
    nix::unistd::gethostname().map_or_else(
        |_| String::from("localhost"),
        |hostname| hostname.to_string_lossy().into_owned(),
    )
}

/// The vi motion a bare key requests, or `None` when the key is not a motion.
///
/// The vocabulary is the subset of Alacritty's motions a terminal copy mode
/// actually needs: `hjkl` plus the arrows for cells, `w` / `b` for words, `0`
/// / `$` for line ends, and `H` / `L` for the viewport edges.
fn vi_motion_for_key(key: &str, shift: bool) -> Option<ViMotion> {
    match key {
        "h" | "left" => Some(ViMotion::Left),
        "j" | "down" => Some(ViMotion::Down),
        "k" | "up" => Some(ViMotion::Up),
        "l" | "right" => Some(ViMotion::Right),
        "w" => Some(ViMotion::WordRight),
        "b" => Some(ViMotion::WordLeft),
        "0" | "home" => Some(ViMotion::First),
        "$" | "end" => Some(ViMotion::Last),
        "^" => Some(ViMotion::FirstOccupied),
        "g" if shift => Some(ViMotion::Low),
        "g" => Some(ViMotion::High),
        _ => None,
    }
}

/// Compile the configured smart-selection rules, warning about the rejects.
///
/// A bad regex is a user-authored mistake that silently removes a context-menu
/// row, so it is surfaced in the log rather than swallowed by the compiler.
fn compile_smart_selection(config: &SmartSelectionConfig) -> CompiledSmartSelection {
    let rules = CompiledSmartSelection::compile(config);
    for error in &rules.errors {
        tracing::warn!(
            rule = %error.rule_id,
            name = %error.rule_name,
            message = %error.message,
            "smart-selection rule rejected",
        );
    }
    rules
}

/// Lower one resolved smart-selection action onto a context-menu row.
fn smart_selection_menu_item(action: ResolvedSmartSelectionAction) -> MenuItem {
    let enabled = !action.parameter.is_empty();
    let label = action.label.clone();
    MenuItem { label, action: smart_selection_action(action), enabled }
}

/// Map a smart-selection action kind onto the context-menu action that runs it.
///
/// One-for-one with the winit client's `smart_selection_context_action`, so a
/// rule authored against the legacy client behaves identically here.
fn smart_selection_action(action: ResolvedSmartSelectionAction) -> ContextMenuAction {
    let parameter = action.parameter;
    match action.kind {
        SmartSelectionActionKind::OpenFile => ContextMenuAction::OpenFile(parameter),
        SmartSelectionActionKind::OpenUrl => ContextMenuAction::OpenUrl(parameter),
        SmartSelectionActionKind::RunCommand => ContextMenuAction::RunCommand(parameter),
        SmartSelectionActionKind::RunCoprocess => ContextMenuAction::RunCoprocess(parameter),
        SmartSelectionActionKind::SendText => ContextMenuAction::SendText(parameter),
        SmartSelectionActionKind::RunCommandInWindow => {
            ContextMenuAction::RunCommandInWindow(parameter)
        }
        SmartSelectionActionKind::Copy => ContextMenuAction::CopyText(parameter),
    }
}

/// Representative active notes seeding the workspace-notes modal demo so the
/// visual E2E can exercise the list, per-row actions, and editor surfaces.
fn demo_workspace_notes(workspace_id: WorkspaceId) -> Vec<WorkspaceNoteEntry> {
    ["Wire up the release checklist", "Follow up on the perf gate numbers"]
        .into_iter()
        .enumerate()
        .map(|(index, text)| WorkspaceNoteEntry {
            note_id: format!("demo-{index}"),
            workspace_id,
            text: text.to_owned(),
            status: WorkspaceNoteStatus::Active,
            created_at_ms: 0,
            updated_at_ms: 0,
            archived_at_ms: None,
            archive_reason: None,
        })
        .collect()
}

/// Build the spawn command for an AI tab, matching the legacy client.
///
/// The CLI starts through the user's login shell (`-lic` + `exec`) so it
/// inherits the same PATH and rc files a normal tab would, without first
/// rendering a shell prompt.
/// Lower a shared [`AutomationAction`] onto the [`KeyAction`] the keybinding
/// path already dispatches, so the command palette and a bound chord converge
/// on one handler.
///
/// `None` marks the three actions with no bindable chord — a profile switch,
/// the update dialog, and session focus — which
/// [`TerminalView::execute_automation_action`] handles directly. The match is
/// exhaustive so a new automation action fails to compile here instead of
/// quietly becoming unroutable.
fn key_action_for_automation(action: &AutomationAction) -> Option<KeyAction> {
    let layout = match action {
        AutomationAction::OpenSettings => return Some(KeyAction::OpenSettings),
        AutomationAction::OpenFind => return Some(KeyAction::OpenFind),
        AutomationAction::NewTab => LayoutAction::NewTab,
        AutomationAction::NewClaudeTab => LayoutAction::NewClaudeTab,
        AutomationAction::NewClaudeResumeTab => LayoutAction::NewClaudeResumeTab,
        AutomationAction::NewCodexTab => LayoutAction::NewCodexTab,
        AutomationAction::NewCodexResumeTab => LayoutAction::NewCodexResumeTab,
        AutomationAction::SplitVertical => LayoutAction::SplitVertical,
        AutomationAction::SplitHorizontal => LayoutAction::SplitHorizontal,
        AutomationAction::ClosePane => LayoutAction::ClosePane,
        AutomationAction::CloseTab => LayoutAction::CloseTab,
        AutomationAction::NewWindow => LayoutAction::NewWindow,
        AutomationAction::SwitchProfile { .. }
        | AutomationAction::OpenUpdateDialog
        | AutomationAction::FocusSession { .. } => return None,
    };
    Some(KeyAction::Layout(layout))
}

/// Login-shell argv for a context-menu "run in a new window" action, matching
/// the legacy client's `shell_command_argv`.
fn shell_command_argv(command: &str) -> Vec<String> {
    vec![scribe_common::shell::default_shell_program(), String::from("-lc"), command.to_owned()]
}

/// Spawn a detached background shell command for a smart-selection coprocess
/// action. Fire-and-forget: the child is never awaited, matching the legacy
/// `spawn_background_shell_command`.
fn spawn_background_command(command: &str) {
    let mut child = std::process::Command::new(scribe_common::shell::default_shell_program());
    child.arg("-lc").arg(command);
    if let Err(error) = child.spawn() {
        tracing::warn!(%error, "smart selection command failed to spawn");
    }
}

fn ai_tab_command(provider: AiProvider, resume: bool) -> Vec<String> {
    let shell = scribe_common::shell::default_shell_program();
    let binary = provider.binary_name();
    let command = if resume {
        format!("exec {binary} {}", provider.resume_args().join(" "))
    } else {
        format!("exec {binary}")
    };
    vec![shell, String::from("-lic"), command]
}

/// Keystroke encoder feeding the outbound [`IpcSink`] (see
/// [`TerminalView::on_key_down`]).
///
/// This is the live entry point of the ported terminal encoder: the GPUI event
/// is lowered by [`KeyInput::from_key_down`] and handed to
/// [`input::encode`](scribe_client_gpui::input::encode), the same function the
/// golden byte fixtures pin. It replaced an interim passthrough table that only
/// knew Enter/Tab/Backspace/Escape and the four arrows, so every other named
/// key — PageUp/PageDown, Home/End, Insert/Delete, the function keys — was
/// silently dropped before the PTY even though the encoder had always mapped
/// them (`CSI 5~` / `CSI 6~` and friends).
///
/// The mode is [`TerminalMode::legacy`] because this client tracks no per-pane
/// DECCKM/DECPAM or Kitty negotiation yet; that state lands with the terminal
/// mode plumbing and is the only thing standing between here and full parity.
fn encode_key(event: &KeyDownEvent) -> Option<Vec<u8>> {
    let text = event.keystroke.key_char.as_deref().filter(|text| !text.is_empty());
    // Multi-codepoint text has no single-key encoder form (the encoder works on
    // one logical key), so forward it verbatim as the interim table did.
    if let Some(text) = text
        && text.chars().count() > 1
    {
        return Some(text.as_bytes().to_vec());
    }
    if let Some(key_input) = KeyInput::from_key_down(event) {
        return input::encode(&key_input, TerminalMode::legacy());
    }
    // The keystroke names no key the encoder knows; if the platform still
    // produced text, that text is the best available encoding.
    text.map(|text| text.as_bytes().to_vec())
}

/// Drains the config watcher's change signal on the GPUI foreground.
///
/// The `notify` callback runs on its own thread and only bumps the shared
/// atomic; this task is the hop back onto the thread that owns the view, where
/// the reload can safely touch entities and request a repaint. It exits when the
/// window is gone.
async fn drive_config_reloads(
    view: WeakEntity<TerminalView>,
    app: &mut AsyncApp,
    signal: ConfigChangeSignal,
) {
    let mut seen = signal.generation();
    loop {
        app.background_executor().timer(CONFIG_POLL_INTERVAL).await;
        if !signal.take_change(&mut seen) {
            continue;
        }
        if view.update(app, TerminalView::reload_config).is_err() {
            return;
        }
    }
}

/// Refreshes the X11 active-window guard on a timer.
///
/// The legacy winit client polled `_NET_ACTIVE_WINDOW` from its event loop so
/// the guard noticed a compositor overlay even while no key events arrived; the
/// GPUI client has no such tick, so the poll gets its own task. It only updates
/// the guard's cached state — suppression is still decided on the key path.
async fn drive_x11_focus_polls(view: WeakEntity<TerminalView>, app: &mut AsyncApp) {
    loop {
        app.background_executor().timer(X11_FOCUS_POLL_INTERVAL).await;
        if view.update(app, TerminalView::poll_x11_focus).is_err() {
            return;
        }
    }
}

/// Runs the window-lifecycle tick on the GPUI foreground.
///
/// The IPC reader learns that the server acknowledged a close or a quit, and
/// moves the focused pane on a reattach, but it owns neither the app nor the
/// sink's UI-side callers. This task is where those cross-thread facts become
/// actions: quitting the app, reporting focus, and re-polling the window list.
async fn drive_window_lifecycle(view: WeakEntity<TerminalView>, app: &mut AsyncApp) {
    loop {
        app.background_executor().timer(WINDOW_LIFECYCLE_TICK).await;
        if view.update(app, TerminalView::poll_window_lifecycle).is_err() {
            return;
        }
    }
}

/// Repaints the view whenever the IPC drain bumps the shared generation counter.
///
/// The same 16 ms tick is the idle-wake boundary the feature-015 control hint
/// expires on: a hint set five seconds ago must clear even on a window whose
/// output has gone quiet, which by definition never bumps the generation.
async fn drive_redraws(
    view: WeakEntity<TerminalView>,
    app: &mut AsyncApp,
    generation: Arc<AtomicU64>,
) {
    let mut rendered = generation.load(Ordering::Acquire);
    loop {
        app.background_executor().timer(Duration::from_millis(16)).await;
        let current = generation.load(Ordering::Acquire);
        if current == rendered {
            if view.update(app, TerminalView::expire_share_hint).is_err() {
                return;
            }
            continue;
        }
        rendered = current;
        if view.update(app, |_, view_cx| view_cx.notify()).is_err() {
            return;
        }
    }
}

impl TerminalView {
    /// Build the status-bar segment model from the live connection / stats
    /// state and the server-reported chrome metadata for the attached pane.
    ///
    /// The metadata segments (workspace, CWD, git branch, env warning, tmux and
    /// host labels) all read from the shared [`ChromeMetadata`] the IPC reader
    /// fills from `CwdChanged` / `GitBranch` / `SessionContextChanged` /
    /// `EnvStatus` / `WorkspaceNamed` and the `SessionList` snapshot, so a
    /// segment is absent only when the server has nothing to report.
    fn build_status_model(&mut self) -> status_bar::StatusBarModel {
        let connected = self.shared.connected.load(Ordering::Acquire);
        let active_session = self.shared.active_session.lock().ok().and_then(|guard| *guard);
        let session_count = usize::from(active_session.is_some());
        // The strip is the only place the attached pane's workspace is known,
        // and it is a different lock from the metadata store, so resolve the id
        // (a `Copy`) and release it before reading the names.
        let workspace_id = active_session.and_then(|session_id| {
            let tabs = self.shared.tabs.lock().ok()?;
            let entry = tabs.tabs().iter().find(|tab| tab.session_id == session_id)?;
            Some(entry.workspace_id)
        });
        // Refresh the sparkline sampler (internally rate-limited to 2 s) and
        // build the full segment model from the live data available so far.
        let sys_stats = self.stats.maybe_refresh();
        let metadata = self.shared.chrome_metadata.lock().ok();
        let metadata = metadata.as_deref();
        let session =
            active_session.zip(metadata).and_then(|(session_id, store)| store.session(session_id));
        // Hold the update guard across `build_model`: `StatusBarData` borrows
        // the version and progress state rather than cloning them per frame.
        let update = self.shared.update.lock().ok();
        let update_available = update.as_ref().and_then(|state| state.version());
        let update_progress = update.as_ref().and_then(|state| state.progress());
        // Same borrow discipline as the update guard: the remote-control
        // summary is rendered from the controller list the last `WindowList`
        // reply left behind, so the lock is held across `build_model`.
        let remote_enabled = self.config.config().config.remote.enabled;
        let lifecycle = self.shared.lifecycle.lock().ok();
        let controllers = lifecycle.as_ref().map_or(&[][..], |state| state.controllers());
        status_bar::build_model(
            &StatusBarData {
                connected,
                workspace_name: workspace_id
                    .zip(metadata)
                    .and_then(|(id, store)| store.workspace_name(id)),
                cwd: session.and_then(|chrome| chrome.cwd.as_deref()),
                git_branch: session.and_then(|chrome| chrome.git_branch.as_deref()),
                last_command_status: None,
                env_status: session.and_then(|chrome| chrome.env_status.as_ref()),
                session_count,
                remote: RemoteStatusData { enabled: remote_enabled, controllers },
                share_presence: self.shared.share.lock().ok().and_then(|share| share.presence()),
                // A remote shell's own host wins; a local pane keeps the
                // placeholder until the hostname surface lands.
                host_label: session.and_then(SessionChrome::host_label).unwrap_or("local"),
                remote_transport: None,
                tmux_label: session.and_then(SessionChrome::tmux_label),
                time: "",
                update_available,
                update_progress,
                sys_stats: Some(sys_stats),
                stats_config: Some(&self.stats_config),
            },
            &self.status_colors,
        )
    }

    /// Build the status-bar band, wiring the centred update CTA's click to the
    /// confirmation modal.
    ///
    /// The palette is cached across renders, so the live opacity is folded into
    /// the filled band here rather than at theme-reload time.
    fn render_status_bar(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let model = self.build_status_model();
        let colors = self.status_colors.with_opacity(self.opacity);
        status_bar::render(
            &model,
            window_chrome::STATUS_BAR_HEIGHT,
            &colors,
            Some(Box::new(cx.listener(|view, _event, _window, ctx| {
                view.open_update_dialog(ctx);
            }))),
        )
        .into_any_element()
    }

    /// Build the attached pane's prompt-bar model, or `None` when the bar is
    /// disabled, no pane is attached, or that pane has no prompts yet.
    ///
    /// The context meter is attached whenever the tracker holds a percentage for
    /// the pane, independent of the warn band — the prompt bar is the surface
    /// that always shows the Ok band, while the tab suffix suppresses it (see
    /// [`Self::sync_tab_context_suffix`]).
    fn build_prompt_model(&self) -> Option<prompt_bar::PromptBarModel> {
        if !self.prompt_bar_enabled {
            return None;
        }
        let session_id = (*self.shared.active_session.lock().ok()?)?;
        let ai = self.shared.ai.lock().ok()?;
        let data = ai.prompts.get(&session_id)?;
        let indicator = ai.tracker.context_for(session_id).map(|percent| {
            PromptContextIndicator::from_thresholds(
                percent,
                &self.context_thresholds,
                self.prompt_colors.text,
            )
        });
        prompt_bar::build_model(data, std::time::SystemTime::now(), indicator)
    }

    /// Push the attached pane's context-% suffix onto its tab, so the warn and
    /// danger bands surface on the tab label as well as in the prompt bar.
    fn sync_tab_context_suffix(&mut self, cx: &mut Context<Self>) {
        let suffix = self.active_tab_context_suffix();
        self.titlebar.update(cx, |bar, ctx| {
            let mut tabs = bar.tabs().to_vec();
            // The suffix belongs to the pane the meter was read from, which is
            // the focused tab — not necessarily the first one now that the tab
            // shortcuts can open and select several.
            let Some(tab) = tabs.iter_mut().find(|tab| tab.is_active) else {
                return;
            };
            if tab.context_suffix == suffix {
                return;
            }
            tab.context_suffix = suffix;
            bar.set_tabs(tabs, ctx);
        });
    }

    /// The context-% suffix for the attached pane's tab, or `None` below the
    /// warn band / while a pulsing attention state owns the UX.
    fn active_tab_context_suffix(&self) -> Option<scribe_client_gpui::tab_bar::ContextSuffix> {
        let session_id = (*self.shared.active_session.lock().ok()?)?;
        let ai = self.shared.ai.lock().ok()?;
        let percent = ai.tracker.context_for(session_id)?;
        context_suffix(
            percent,
            self.context_thresholds.warn,
            self.context_thresholds.danger,
            ai.tracker.context_suffix_suppressed(session_id),
        )
    }

    /// Lower the live share state onto the overlay layer: the presence roster,
    /// the transient control hint, and the modal grant/deny prompt.
    /// Build the terminal-grid band: the pane layout's canvas.
    ///
    /// Every pane is positioned inside it as a fraction of its size, so the
    /// split ratios need no device-pixel measurement, and the band itself
    /// carries the right-click that opens the context menu at the cursor
    /// without disturbing the display-only elements inside it.
    fn render_grid(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let focused = self.sync_split_scroll();
        let panes = self.render_panes(focused, cx);
        // Measure the flex-grown grid band. Its height is whatever the chrome
        // bands leave over, which no arithmetic on the window size can predict
        // (the prompt strip comes and goes with the pane's prompts), so the
        // painted rect is the only honest source for the cell counts the server
        // is told about.
        let area = Rc::clone(&self.grid_area);
        div()
            .flex_1()
            .relative()
            .bg(surface(self.terminal_colors.background, self.opacity))
            .child(
                canvas(move |bounds, _window, _cx| area.set(Some(bounds)), |_, (), _, _| {})
                    .absolute()
                    .size_full(),
            )
            .children(panes)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|view, event: &MouseDownEvent, _window, ctx| {
                    view.click_grid(event.position, ctx);
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|view, event: &MouseDownEvent, _window, ctx| {
                    view.open_context_menu(event.position, ctx);
                }),
            )
            .into_any_element()
    }

    /// Lower the live share state onto the overlay layer: the presence roster,
    /// the transient control hint, and the modal grant/deny prompt.
    fn build_share_overlay(&self) -> Option<gpui::AnyElement> {
        let colors = ShareOverlayColors::from(&self.chrome);
        let share = self.shared.share.lock().ok()?;
        share_overlay(&share, &colors)
    }

    /// Build the demo hover tooltip when the `tooltip_demo` toggle is on: a long
    /// URI anchored near the right edge, exercising both the head+tail truncation
    /// and the viewport clamp.
    fn build_tooltip_demo(&self) -> Option<gpui::AnyElement> {
        self.tooltip_demo.then(|| {
            let colors = TooltipColors::from(&self.chrome);
            let anchor = Rect { x: 780.0, y: 120.0, width: 120.0, height: 18.0 };
            let display = scribe_client_gpui::tooltip::truncate_url(
                "https://example.com/very/long/path/that/overflows/the/box",
                48,
            );
            tooltip_element(&TooltipRender {
                text: &display,
                anchor,
                position: TooltipPosition::Below,
                viewport_width: f32::from(CELL_WIDTH) * f32::from(COLUMNS),
                colors: &colors,
                char_width: f32::from(CELL_WIDTH),
                line_height: f32::from(CELL_HEIGHT),
            })
        })
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        log_first_frame_timing();
        self.report_perf_frame();
        if !self.focus_handle.is_focused(window) {
            window.focus(&self.focus_handle, cx);
        }
        self.sync_tabs(cx);
        self.sync_find_results(cx);
        self.reconcile_panes(cx);
        self.sync_grid_geometry(cx);
        let grid = self.render_grid(cx);
        let status = self
            .shared
            .status
            .lock()
            .map_or_else(|_| "terminal state unavailable".to_owned(), |guard| guard.clone());

        let status_bar = self.render_status_bar(cx);
        let prompt_model = self.build_prompt_model();
        self.sync_tab_context_suffix(cx);
        let prompt_colors = self.prompt_colors.with_opacity(self.opacity);
        let prompt_strip = prompt_model.map(|prompt| {
            prompt_bar::render(&prompt, &prompt_colors, f32::from(CELL_HEIGHT), None)
                .into_any_element()
        });
        let tooltip = self.build_tooltip_demo();
        let share = self.build_share_overlay();

        let opacity = self.opacity;
        // The root itself paints nothing. Every band below fills the window
        // edge to edge, so leaving the root unfilled guarantees each pixel
        // carries the opacity alpha exactly once instead of compositing a
        // translucent band over a translucent root and coming out more opaque
        // than the configured value.
        div()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|view, event: &KeyDownEvent, _window, ctx| {
                // The X11 active-window guard gates everything: while a
                // compositor overlay owns the screen the keystroke was never
                // meant for this window, so it reaches no consumer at all.
                if view.compositor_overlay_active(event) {
                    return;
                }
                // Overlays own the keyboard first, then the configured
                // bindings, and only then the generic PTY byte encoder.
                if view.handle_overlay_key(event, ctx)
                    || view.handle_vi_key(event, ctx)
                    || view.handle_binding(event, ctx)
                {
                    return;
                }
                view.on_key_down(event, ctx);
            }))
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .child(self.titlebar.clone())
            .child(grid)
            .children(prompt_strip)
            .child(
                // `flex_none` on every band below the grid: the grid is the one
                // flex-grown child, so without it a window shorter than the
                // grid's painted height would shrink the bands away instead of
                // clipping the grid, taking the status surfaces off screen.
                div()
                    .flex_none()
                    .h(px(window_chrome::STATUS_STRIP_HEIGHT))
                    .px_2()
                    .flex()
                    .items_center()
                    .bg(surface(self.chrome.status_bar_bg, opacity))
                    .text_color(opaque_slot(self.chrome.status_bar_text))
                    .text_xs()
                    .child(status),
            )
            .child(status_bar)
            .children(self.command_palette.clone())
            .children(self.find_overlay.clone())
            .children(self.context_menu.clone())
            .children(self.dialog.clone())
            .children(self.workspace_notes_modal.clone())
            .children(share)
            .children(tooltip)
    }
}

/// Install the `tracing` subscriber, mirroring the legacy client's setup so the
/// GPUI client's diagnostics (config hot-reload, dropped IPC sends, watcher
/// failures) actually reach stderr instead of being discarded. `RUST_LOG`
/// overrides the default `info` filter.
fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
}

/// The startup grid geometry every terminal window is created with.
fn default_terminal_size() -> TerminalSize {
    TerminalSize { cols: COLUMNS, rows: ROWS, cell_width: CELL_WIDTH, cell_height: CELL_HEIGHT }
}

/// The inner size a new terminal window opens at.
///
/// Derived rather than hardcoded: the window has to fit the whole
/// [`COLUMNS`]x[`ROWS`] grid *at the metrics the grid is painted with* plus
/// every chrome band, otherwise the flex column silently clips whichever comes
/// last. The old fixed 960x680 was the grid's painted height alone (36 rows x
/// 18.9 px) with nothing left for the 84 px of titlebar, status strip and
/// status bar, so the bottom five rows were cut off and a slightly smaller
/// window would have taken the bands with them. The result is clamped to the
/// display so a large `appearance.font_size` cannot push the status bar off the
/// screen instead of off the window.
fn startup_window_size(cx: &App) -> Size<Pixels> {
    let appearance = load_config().unwrap_or_default().appearance;
    let font = GridFont::from_appearance(&appearance);
    let wanted =
        window_chrome::default_window_size(COLUMNS, ROWS, font.cell_width(), font.line_height);
    let wanted = cx.primary_display().map_or(wanted, |display| {
        let bounds = display.bounds().size;
        window_chrome::clamp_to_display(
            wanted,
            window_chrome::WindowSize {
                width: f32::from(bounds.width),
                height: f32::from(bounds.height),
            },
        )
    });
    size(px(wanted.width), px(wanted.height))
}

/// Build one terminal window's backend: fresh shared state plus its own IPC
/// connection to the server, with the reader/writer thread already running.
///
/// Every window owns an independent [`Shared`] — its own grid, status line, tab
/// strip, and chrome — because a window is a separate client from the server's
/// point of view: the `Hello` this connection sends carries no `window_id`, so
/// the server registers a *new* window and attaches its own sessions to it.
/// Sharing one `Shared` between windows would instead mirror a single session
/// strip into both, which is not what "new window" means.
///
/// Called once from [`main`] for the startup window and again for every
/// [`LayoutAction::NewWindow`], so the two paths cannot drift.
fn start_window_backend(terminal_size: TerminalSize) -> (Shared, IpcSink) {
    let shared = Shared {
        panes: Arc::new(Mutex::new(PaneGrids::new(usize::from(COLUMNS), usize::from(ROWS)))),
        attached: Arc::new(Mutex::new(HashSet::new())),
        status: Arc::new(Mutex::new("connecting to Scribe server…".to_owned())),
        generation: Arc::new(AtomicU64::new(0)),
        active_session: Arc::new(Mutex::new(None)),
        tabs: Arc::new(Mutex::new(TabSessions::new())),
        connected: Arc::new(AtomicBool::new(false)),
        ai: Arc::new(Mutex::new(AiChrome::new(
            load_config().unwrap_or_default().terminal.ai_session.ai_states,
        ))),
        chrome_metadata: Arc::new(Mutex::new(ChromeMetadata::new())),
        share: Arc::new(Mutex::new(ShareChrome::new())),
        update: Arc::new(Mutex::new(UpdateState::default())),
        lifecycle: Arc::new(Mutex::new(WindowLifecycle::new())),
        bells: Arc::new(Mutex::new(Vec::new())),
        find: Arc::new(Mutex::new(FindResults::default())),
        lan: Arc::new(Mutex::new(LanChrome::new())),
    };
    let (out_tx, out_rx) = unbounded_channel::<ClientMessage>();
    let (in_tx, in_rx) = unbounded_channel::<InboundEvent>();
    let sink = IpcSink::new(out_tx.clone());

    start_ipc_thread(IpcThread {
        shared: shared.clone(),
        sink: sink.clone(),
        out_tx,
        out_rx,
        in_tx,
        in_rx,
        size: terminal_size,
    });

    (shared, sink)
}

fn main() {
    PROCESS_START.get_or_init(Instant::now);
    // Arm the perf rig's runtime probe before anything can paint or type; it
    // stays inert unless `SCRIBE_PERF_PROBE` names a report path.
    scribe_common::perf_probe::init_from_env();
    init_tracing();

    // `scribe-client --settings` opens (or focuses) the settings window instead
    // of the terminal shell. The singleton absorbs the old scribe-settings
    // `settings.lock`/`settings.sock`: a second launch hands focus to the
    // running window and exits here.
    if std::env::args().skip(1).any(|arg| arg == "--settings") {
        run_settings();
        return;
    }

    let terminal_size = default_terminal_size();
    let (shared, sink) = start_window_backend(terminal_size);

    application().run(move |cx: &mut App| {
        // Register the embedded Symbols Nerd Font before anything shapes a
        // line: `load_family` caches per-family lookups, so a later add could
        // never displace a cached miss for the fallback chain's first entry.
        scribe_client_gpui::fonts::register_embedded_fonts(cx);
        // Resolve the motion policy from `appearance.animations` (default true)
        // and the SCRIBE_DISABLE_ANIMATIONS override, then mirror it onto GPUI's
        // global reduce-motion flag so any UI transitions stay off — and
        // screenshots stay byte-identical — under the E2E determinism path.
        let animations = load_config().map_or(true, |config| config.appearance.animations);
        AnimationSettings::resolve(animations).apply_to_app(cx);
        open_window(cx, &shared, &sink, terminal_size);
        cx.activate(true);
    });
}

/// Run the settings-only flow for `--settings`: enforce the singleton, then
/// open the GPUI settings window. When another instance already holds the
/// socket, [`singleton::acquire`] hands it focus and we exit without opening a
/// duplicate window.
fn run_settings() {
    use scribe_client_gpui::settings::singleton::{self, SingletonResult};

    let (listener, socket_path, lock_file) = match singleton::acquire(None) {
        Ok(SingletonResult::Primary { listener, socket_path, lock_file }) => {
            (listener, socket_path, lock_file)
        }
        Ok(SingletonResult::AlreadyRunning) => {
            tracing::info!("settings window already running; sent focus and exiting");
            return;
        }
        Err(e) => {
            tracing::error!("failed to acquire settings singleton: {e}");
            return;
        }
    };

    application().run(move |cx: &mut App| {
        let animations = load_config().map_or(true, |config| config.appearance.animations);
        AnimationSettings::resolve(animations).apply_to_app(cx);
        scribe_client_gpui::settings::open_settings_window(cx);
        cx.activate(true);
    });

    // Hold the singleton guards for the window's lifetime, then clean up.
    singleton::cleanup_socket(&socket_path);
    drop(listener);
    drop(lock_file);
}

fn open_window(cx: &mut App, shared: &Shared, sink: &IpcSink, terminal_size: TerminalSize) {
    let bounds = Bounds::centered(None, startup_window_size(cx), cx);
    let shared = shared.clone();
    let sink = sink.clone();
    // Everything between here and the root-view builder below happens inside
    // gpui: window creation, wgpu adapter enumeration, device creation and
    // surface configure. Timing it separates the platform GPU bring-up floor
    // from Scribe's own startup work for the perf gate.
    let bringup_start = Instant::now();
    if let Err(error) = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            // Set WM_NAME/_NET_WM_NAME to "Scribe" so the X11 visual-E2E harness
            // (`docker/entrypoint-visual.sh`) can locate the window with
            // `xdotool search --name "Scribe"` for focus and screenshot capture.
            titlebar: Some(TitlebarOptions { title: Some("Scribe".into()), ..Default::default() }),
            app_id: Some("scribe".to_owned()),
            // Always ask for an alpha-capable surface, even at opacity 1.0.
            // Surface capability is fixed when the window is created, so
            // choosing it from the startup opacity would force a restart to
            // ever go translucent — the legacy client's `window_transparent`
            // wart. Requesting it unconditionally keeps `appearance.opacity`
            // a pure repaint; at 1.0 every painted background is alpha 1.0
            // and the window is pixel-identical to an opaque one.
            window_background: WindowBackgroundAppearance::Transparent,
            ..Default::default()
        },
        |window, cx| {
            let bringup_ms = bringup_start.elapsed().as_secs_f64() * 1000.0;
            WINDOW_BRINGUP_MS_BITS.store(bringup_ms.to_bits(), Ordering::Release);
            cx.new(|cx| TerminalView::new(shared, sink, terminal_size, window, cx))
        },
    ) {
        tracing::error!(%error, "failed to open GPUI window");
    }
}

/// Everything the background IPC thread owns for one connection.
struct IpcThread {
    shared: Shared,
    sink: IpcSink,
    out_tx: UnboundedSender<ClientMessage>,
    out_rx: UnboundedReceiver<ClientMessage>,
    in_tx: UnboundedSender<InboundEvent>,
    in_rx: UnboundedReceiver<InboundEvent>,
    size: TerminalSize,
}

fn start_ipc_thread(ctx: IpcThread) {
    std::thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(runtime) => runtime,
            Err(error) => {
                set_status(
                    &ctx.shared.status,
                    &ctx.shared.generation,
                    format!("IPC runtime failed: {error}"),
                );
                return;
            }
        };
        runtime.block_on(async move {
            let status = Arc::clone(&ctx.shared.status);
            let generation = Arc::clone(&ctx.shared.generation);
            let connected = Arc::clone(&ctx.shared.connected);
            if let Err(error) = run_connection(ctx).await {
                connected.store(false, Ordering::Release);
                set_status(&status, &generation, format!("server connection failed: {error}"));
            }
        });
    });
}

/// Establish this client's one connection and serve it until it closes.
///
/// Two transports reach the same server-side protocol. The default is the local
/// Unix socket; `SCRIBE_LAN_DIAL` instead points the client at a peer on the
/// local network, which is reached over TCP + pinned mutual TLS and gated by the
/// owning side's device approval before a single byte of window state flows
/// (feature 014). Past that gate the streams are interchangeable, so both paths
/// converge on [`serve_connection`].
async fn run_connection(ctx: IpcThread) -> Result<(), String> {
    match lan_dial::target_from_env() {
        Some((host, port)) => run_lan_connection(ctx, host, port).await,
        None => run_local_connection(ctx).await,
    }
}

/// Connect to this machine's own server over the local Unix socket.
///
/// The LAN environment is probed FIRST, before the session connection exists.
/// `GetLanEnv` is a pre-`Hello` first frame the server answers on its own
/// transient socket, so it has to be a separate connection either way; running
/// it up front means the window has its LAN summary before the first frame
/// paints, and it leaves the session connection as the last socket this process
/// opened, which is what the E2E wire tap addresses the client by.
async fn run_local_connection(ctx: IpcThread) -> Result<(), String> {
    let lan_env = probe_lan_env().await;
    let stream = tokio::net::UnixStream::connect(server_socket_path())
        .await
        .map_err(|error| error.to_string())?;
    // Socket is up: light the status-bar connection dot green.
    ctx.shared.connected.store(true, Ordering::Release);
    let (reader, writer) = stream.into_split();
    serve_connection(ctx, reader, writer, Transport::Local(lan_env)).await
}

/// Dial a LAN peer over TCP + pinned mutual TLS, run the `LanHello` preamble and
/// the owning side's device-approval gate, and only then serve the connection.
///
/// Every failure short of acceptance is terminal for this process: the client
/// was launched to control that peer, so falling back to the local server would
/// silently attach the user to the wrong machine.
async fn run_lan_connection(ctx: IpcThread, host: String, port: u16) -> Result<(), String> {
    let lan = Arc::clone(&ctx.shared.lan);
    let status = Arc::clone(&ctx.shared.status);
    let generation = Arc::clone(&ctx.shared.generation);
    tracing::info!(%host, port, "dialing a LAN peer instead of the local socket");

    let dialer = LanDialer::build(host, port).await.map_err(|error| error.to_string())?;
    let mut stream = dialer.connect().await.map_err(|error| error.to_string())?;

    // The gate can block on a human for as long as the peer's approval hold
    // allows, so the interim "waiting" state is pushed to the window the moment
    // the peer reports it rather than after the decision.
    let pending_lan = Arc::clone(&lan);
    let pending_status = Arc::clone(&status);
    let pending_generation = Arc::clone(&generation);
    let outcome = lan_dial::handshake(&mut stream, lan_dial::local_device_name(), move || {
        publish_lan_status(
            &pending_lan,
            &pending_status,
            &pending_generation,
            LanChrome::awaiting_approval,
        );
    })
    .await;

    publish_lan_status(&lan, &status, &generation, |chrome| chrome.settle_dial(outcome));
    if outcome != LanConnectOutcome::Accepted {
        let (peer_host, peer_port) = dialer.target();
        return Err(format!("LAN dial to {peer_host}:{peer_port} was not accepted"));
    }

    // Approved: the encrypted link now behaves exactly like the local socket.
    ctx.shared.connected.store(true, Ordering::Release);
    let (reader, writer) = tokio::io::split(stream);
    serve_connection(ctx, reader, writer, Transport::Lan).await
}

/// Apply `mutate` to the LAN chrome and mirror the resulting summary onto the
/// status bar.
///
/// The dial path runs before any [`ReaderCtx`] exists, so it holds the three
/// shared handles directly rather than going through [`update_lan_chrome`].
fn publish_lan_status(
    lan: &Arc<Mutex<LanChrome>>,
    status: &Arc<Mutex<String>>,
    generation: &Arc<AtomicU64>,
    mutate: impl FnOnce(&mut LanChrome),
) {
    let Ok(mut guard) = lan.lock() else {
        tracing::warn!("LAN chrome mutex poisoned; dropping dial update");
        return;
    };
    mutate(&mut guard);
    let line = guard.status_line();
    drop(guard);
    if let Some(line) = line {
        set_status(status, generation, line);
    }
}

/// Which transport carried this connection, gating the local-only LAN queries.
/// A remote peer must never be asked to enumerate this machine's LAN view — the
/// server refuses it, and asking would put a pointless frame on the wire.
enum Transport {
    /// This machine's own server over the Unix socket, carrying the `LanEnv`
    /// answer probed before the connection was opened. `None` when LAN access is
    /// off or the probe failed.
    Local(Option<ServerMessage>),
    /// A LAN peer over mutual TLS.
    Lan,
}

/// Send the handshake, start the writer and the drain, run the local-only LAN
/// startup probes, and hand the read half to the live reader.
///
/// Shared by both transports so the LAN path can never drift from the local one
/// in what it announces or which state it wires up.
async fn serve_connection<R, W>(
    ctx: IpcThread,
    reader: R,
    writer: W,
    transport: Transport,
) -> Result<(), String>
where
    R: AsyncReadExt + Unpin,
    W: AsyncWriteExt + Unpin + Send + 'static,
{
    // Handshake is queued ahead of any sink traffic on the same ordered channel.
    // `SCRIBE_JOIN_WINDOW` (unset for a user-launched client) names a window
    // another local process already holds: the server resolves that non-takeover
    // claim as an additive share join under any non-`single_controller` sharing
    // mode, so this client renders and types into the SAME panes instead of
    // opening an empty window of its own.
    let join_window = scribe_client_gpui::share_join::join_window_from_env();
    if let Some(window_id) = join_window {
        tracing::info!(%window_id, "joining an existing window's share");
    }
    ctx.out_tx
        .send(ClientMessage::Hello {
            window_id: join_window,
            clipboard_gating: false,
            takeover: false,
        })
        .map_err(|_| "writer channel closed".to_owned())?;
    ctx.out_tx.send(ClientMessage::ListSessions).map_err(|_| "writer channel closed".to_owned())?;

    tokio::spawn(run_writer(writer, ctx.out_rx));
    spawn_drain(ctx.in_rx, Arc::clone(&ctx.shared.panes), Arc::clone(&ctx.shared.generation));

    let reader_ctx = ReaderCtx {
        panes: ctx.shared.panes,
        attached: ctx.shared.attached,
        status: ctx.shared.status,
        generation: ctx.shared.generation,
        active_session: ctx.shared.active_session,
        ai: ctx.shared.ai,
        chrome_metadata: ctx.shared.chrome_metadata,
        tabs: ctx.shared.tabs,
        share: ctx.shared.share,
        update: ctx.shared.update,
        lifecycle: ctx.shared.lifecycle,
        bells: ctx.shared.bells,
        find: ctx.shared.find,
        lan: ctx.shared.lan,
        out_tx: ctx.out_tx,
        in_tx: ctx.in_tx,
        sink: ctx.sink,
        size: ctx.size,
    };
    if let Transport::Local(lan_env) = transport {
        adopt_lan_surface(&reader_ctx, lan_env);
    }
    run_reader(reader, reader_ctx).await
}

/// Ask this machine's own server for its LAN environment on a transient socket.
///
/// Gated on `remote.lan.enabled` exactly as the window-list poll is gated on
/// `remote.enabled`: with LAN access off there is no identity to report and no
/// discovery running, so the frame would be pure noise. The raw reply travels
/// back so it can be folded through the same [`on_lan_message`] the live reader
/// uses rather than a second, divergent parser.
async fn probe_lan_env() -> Option<ServerMessage> {
    if !load_config().is_ok_and(|config| config.remote.lan.enabled) {
        return None;
    }
    match lan_dial::probe_lan_env().await {
        Ok(message) => Some(message),
        Err(error) => {
            tracing::warn!(%error, "LAN environment probe failed");
            None
        }
    }
}

/// Fold the pre-probed `LanEnv` and ask the session connection for the peer
/// list, which the server answers only for a LOCAL connection.
fn adopt_lan_surface(ctx: &ReaderCtx, lan_env: Option<ServerMessage>) {
    let Some(lan_env) = lan_env else {
        return;
    };
    on_lan_message(ctx, lan_env);
    if let Err(error) = ctx.sink.list_lan_peers() {
        tracing::warn!(%error, "LAN peer list request dropped: IPC writer closed");
    }
}

/// Drains the IPC-writer channel to the socket in FIFO order.
async fn run_writer<W>(mut writer: W, mut out_rx: UnboundedReceiver<ClientMessage>)
where
    W: AsyncWriteExt + Unpin,
{
    while let Some(message) = out_rx.recv().await {
        if let Err(error) = write_message(&mut writer, &message).await {
            tracing::warn!(%error, "IPC writer task stopped");
            return;
        }
    }
}

/// Per-session synchronized-output frame queues shared between the coalescing
/// drain (which enqueues committed bursts) and the expiry flusher.
type SyncFrameQueues = Arc<Mutex<HashMap<SessionId, SyncFrameQueue>>>;

/// Spawns the coalescing drain with synchronized-frame queueing in front of
/// `write_output`. A first task drains the inbound channel with Zed's
/// 4 ms / 100-event coalescing ([`run_drain`]), splits each pane's bytes into
/// committed `CSI ? 2026` bursts via a per-session [`SyncFrameQueue`], and
/// replays one burst per redraw so no frame tears across IPC message
/// boundaries. A second task waits on the nearest raw-frame or parser sync
/// deadline and flushes a 150 ms-expired update whose terminating `CSI ? 2026 l`
/// never arrived.
fn spawn_drain(
    in_rx: UnboundedReceiver<InboundEvent>,
    panes: Arc<Mutex<PaneGrids>>,
    generation: Arc<AtomicU64>,
) {
    let queues: SyncFrameQueues = Arc::new(Mutex::new(HashMap::new()));
    let expiry_wake = Arc::new(Notify::new());

    let queue_task = Arc::clone(&queues);
    let panes_task = Arc::clone(&panes);
    let generation_task = Arc::clone(&generation);
    let wake_task = Arc::clone(&expiry_wake);
    tokio::spawn(run_drain(in_rx, move |batch| {
        if batch.is_empty() {
            return;
        }
        let mut redraws = 0usize;
        let mut sync_armed = false;
        if let (Ok(mut session_queues), Ok(mut grids)) = (queue_task.lock(), panes_task.lock()) {
            for (session, bytes) in batch.iter() {
                let queue = session_queues.entry(session).or_default();
                queue.queue_output_frames(bytes);
                // Each pane advances its own grid, so a background pane's burst
                // can never land in the focused pane's scrollback.
                let grid = grids.grid_mut(session);
                redraws += usize::from(drain_all_committed(queue, grid).needs_redraw);
                sync_armed |= grid.parser_sync_deadline().is_some();
                sync_armed |= queue.raw_sync_deadline().is_some();
            }
        }
        for _ in 0..redraws {
            generation_task.fetch_add(1, Ordering::Release);
        }
        if sync_armed {
            wake_task.notify_one();
        }
    }));

    tokio::spawn(run_sync_expiry(queues, panes, generation, expiry_wake));
}

/// Waits on the nearest synchronized-update deadline and commits it once it
/// expires, so an unterminated `CSI ? 2026 h` still flushes after 150 ms even
/// while the inbound channel is idle. When nothing is buffering, it parks until
/// the drain task arms a fresh deadline.
async fn run_sync_expiry(
    queues: SyncFrameQueues,
    panes: Arc<Mutex<PaneGrids>>,
    generation: Arc<AtomicU64>,
    wake: Arc<Notify>,
) {
    loop {
        match next_sync_deadline(&queues, &panes) {
            None => wake.notified().await,
            Some(deadline) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                tokio::select! {
                    () = tokio::time::sleep(remaining) => flush_expired_sync(&queues, &panes, &generation),
                    () = wake.notified() => {}
                }
            }
        }
    }
}

/// Commits every raw-frame and parser synchronized update whose deadline has
/// passed, bumping the redraw generation once per committed burst.
fn flush_expired_sync(
    queues: &SyncFrameQueues,
    panes: &Arc<Mutex<PaneGrids>>,
    generation: &Arc<AtomicU64>,
) {
    let now = Instant::now();
    let mut redraws = 0usize;
    if let (Ok(mut session_queues), Ok(mut grids)) = (queues.lock(), panes.lock()) {
        redraws += grids.flush_parser_sync_timeouts(now);
        for (session, queue) in session_queues.iter_mut() {
            let flushed = queue.flush_raw_timeout(now)
                && drain_all_committed(queue, grids.grid_mut(*session)).needs_redraw;
            redraws += usize::from(flushed);
        }
    }
    for _ in 0..redraws {
        generation.fetch_add(1, Ordering::Release);
    }
}

/// Nearest synchronized-update deadline across every pane's raw-frame queue and
/// every pane's parser, or `None` when nothing is buffering.
fn next_sync_deadline(queues: &SyncFrameQueues, panes: &Arc<Mutex<PaneGrids>>) -> Option<Instant> {
    let parser = panes.lock().ok().and_then(|grids| grids.parser_sync_deadline());
    let raw = queues
        .lock()
        .ok()
        .and_then(|queues| queues.values().filter_map(SyncFrameQueue::raw_sync_deadline).min());
    match (parser, raw) {
        (Some(parser), Some(raw)) => Some(parser.min(raw)),
        (Some(only), None) | (None, Some(only)) => Some(only),
        (None, None) => None,
    }
}

/// Handles owned by the inbound read loop.
struct ReaderCtx {
    /// Per-session display grids, so an exited session's grid can be dropped.
    panes: Arc<Mutex<PaneGrids>>,
    /// Sessions the client has attached; the pane-output gate reads it.
    attached: Arc<Mutex<HashSet<SessionId>>>,
    status: Arc<Mutex<String>>,
    generation: Arc<AtomicU64>,
    active_session: Arc<Mutex<Option<SessionId>>>,
    /// AI state + prompt history the chrome renders from.
    ai: Arc<Mutex<AiChrome>>,
    /// Server-reported terminal chrome the status bar renders from.
    chrome_metadata: Arc<Mutex<ChromeMetadata>>,
    /// Ordered tab strip the reader rebuilds from server session traffic.
    tabs: Arc<Mutex<TabSessions>>,
    /// Feature-015 share state the reader folds roster and control notices into.
    share: Arc<Mutex<ShareChrome>>,
    /// Update availability / progress the centred status-bar CTA renders from.
    update: Arc<Mutex<UpdateState>>,
    /// Window-lifecycle state the reader adopts a window id into and folds the
    /// server's close / quit / window-list answers onto.
    lifecycle: Arc<Mutex<WindowLifecycle>>,
    /// Bell queue the reader appends to and the foreground's [`BellController`]
    /// drains; see the `bells` field of `Shared`.
    bells: Arc<Mutex<Vec<SessionId>>>,
    /// Latest `SearchResults` reply the find overlay renders and highlights.
    find: Arc<Mutex<FindResults>>,
    /// Feature-014 LAN state the reader parks approval requests in and folds the
    /// peer-list, environment, and dial-gate answers onto.
    lan: Arc<Mutex<LanChrome>>,
    out_tx: UnboundedSender<ClientMessage>,
    in_tx: UnboundedSender<InboundEvent>,
    sink: IpcSink,
    size: TerminalSize,
}

/// Apply `mutate` to the shared AI chrome and request a repaint.
///
/// A poisoned mutex is dropped silently rather than propagated: losing an AI
/// indicator update must never tear down the reader and with it the pane's
/// terminal output.
fn update_ai_chrome(ctx: &ReaderCtx, mutate: impl FnOnce(&mut AiChrome)) {
    let Ok(mut guard) = ctx.ai.lock() else {
        tracing::warn!("AI chrome mutex poisoned; dropping update");
        return;
    };
    mutate(&mut guard);
    drop(guard);
    ctx.generation.fetch_add(1, Ordering::Release);
}

/// Apply `mutate` to the shared terminal-chrome metadata and request a repaint.
///
/// Same failure contract as [`update_ai_chrome`]: a poisoned mutex costs one
/// status-bar segment update, never the pane's output stream.
fn update_chrome_metadata(ctx: &ReaderCtx, mutate: impl FnOnce(&mut ChromeMetadata)) {
    let Ok(mut guard) = ctx.chrome_metadata.lock() else {
        tracing::warn!("chrome metadata mutex poisoned; dropping update");
        return;
    };
    mutate(&mut guard);
    drop(guard);
    ctx.generation.fetch_add(1, Ordering::Release);
}

/// Apply `mutate` to the shared feature-015 share chrome and request a repaint.
///
/// Like [`update_ai_chrome`], a poisoned mutex is dropped silently: losing a
/// roster update must never tear down the reader and with it the pane's output.
fn update_share_chrome(ctx: &ReaderCtx, mutate: impl FnOnce(&mut ShareChrome)) {
    let Ok(mut guard) = ctx.share.lock() else {
        tracing::warn!("share chrome mutex poisoned; dropping update");
        return;
    };
    mutate(&mut guard);
    drop(guard);
    ctx.generation.fetch_add(1, Ordering::Release);
}

/// Apply `mutate` to the shared update state and request a repaint.
///
/// Poisoning is dropped silently for the same reason the AI chrome drops it:
/// losing an update banner must never tear down the reader and with it every
/// attached pane's output.
fn update_update_state(ctx: &ReaderCtx, mutate: impl FnOnce(&mut UpdateState)) {
    let Ok(mut guard) = ctx.update.lock() else {
        tracing::warn!("update state mutex poisoned; dropping update");
        return;
    };
    mutate(&mut guard);
    drop(guard);
    ctx.generation.fetch_add(1, Ordering::Release);
}

/// Apply `mutate` to the shared window-lifecycle state and request a repaint.
///
/// Same failure contract as [`update_ai_chrome`]: a poisoned mutex costs one
/// lifecycle update rather than the reader and every pane's output with it. The
/// repaint matters even for the shutdown arms, because the foreground's
/// lifecycle tick is what turns an acknowledged exit into a real quit.
fn update_lifecycle(ctx: &ReaderCtx, mutate: impl FnOnce(&mut WindowLifecycle)) {
    let Ok(mut guard) = ctx.lifecycle.lock() else {
        tracing::warn!("window lifecycle mutex poisoned; dropping update");
        return;
    };
    mutate(&mut guard);
    drop(guard);
    ctx.generation.fetch_add(1, Ordering::Release);
}

/// Apply `mutate` to the shared feature-014 LAN chrome and request a repaint.
///
/// Same failure contract as [`update_ai_chrome`]. The repaint matters here for
/// the same reason it does for the lifecycle state: the foreground's tick is
/// what turns a parked approval request into a real modal on screen.
fn update_lan_chrome(ctx: &ReaderCtx, mutate: impl FnOnce(&mut LanChrome)) {
    let Ok(mut guard) = ctx.lan.lock() else {
        tracing::warn!("LAN chrome mutex poisoned; dropping update");
        return;
    };
    mutate(&mut guard);
    drop(guard);
    ctx.generation.fetch_add(1, Ordering::Release);
}

/// Fold a LAN change and mirror the resulting one-line summary onto the status
/// bar, so every LAN transition the reader applies is also visible on screen.
fn update_lan_chrome_and_status(ctx: &ReaderCtx, mutate: impl FnOnce(&mut LanChrome)) {
    update_lan_chrome(ctx, mutate);
    let line = ctx.lan.lock().ok().and_then(|lan| lan.status_line());
    if let Some(line) = line {
        set_status(&ctx.status, &ctx.generation, line);
    }
}

/// Running total of inbound [`ServerMessage`]s the live reader dropped.
///
/// Incremented by [`unhandled_server_message`]. The GPUI client acts on a
/// minority of the protocol, and before this counter existed the rest vanished
/// into a `_ => {}` arm, so an unimplemented surface was indistinguishable from
/// a working one. A non-zero value after an end-to-end run is the signal that a
/// feature is present on the wire and absent from the client.
static UNHANDLED_SERVER_MESSAGES: AtomicU64 = AtomicU64::new(0);

/// Running total of [`LayoutAction`]s the shell intercepted but cannot run.
///
/// Incremented by [`unhandled_layout_action`]. The key path claims a bound
/// chord before it can reach the PTY, so a swallowed action is invisible to the
/// user: nothing happens and nothing is typed.
static UNHANDLED_LAYOUT_ACTIONS: AtomicU64 = AtomicU64::new(0);

/// Name, count, and warn about an inbound message the live reader drops.
fn unhandled_server_message(message: &ServerMessage) {
    let dropped = UNHANDLED_SERVER_MESSAGES.fetch_add(1, Ordering::Relaxed) + 1;
    tracing::warn!(
        variant = server_message_variant(message),
        dropped,
        "server message not wired into the GPUI client"
    );
}

/// Count and warn about a bound layout action the shell cannot execute.
fn unhandled_layout_action(action: LayoutAction) {
    let dropped = UNHANDLED_LAYOUT_ACTIONS.fetch_add(1, Ordering::Relaxed) + 1;
    tracing::warn!(?action, dropped, "layout action not wired into the GPUI shell");
}

/// Running total of overlay-originated actions that reached a dispatcher with
/// no live handler behind it.
///
/// Distinct from [`UNHANDLED_LAYOUT_ACTIONS`]: those are chords the key path
/// swallowed, these are palette rows, context-menu rows, and key actions whose
/// destination surface (settings window, find overlay, remote picker, update
/// dialog, clipboard) is not ported yet. Both are user-invisible without a log
/// line, which is why every drop is named and counted here.
static UNROUTABLE_ACTIONS: AtomicU64 = AtomicU64::new(0);

/// The ring a pane is drawn with: the owning region's accent when it has focus,
/// the theme's divider otherwise.
///
/// Tinting the focused pane with its *region's* accent is what makes a
/// two-region window readable at a glance: the ring says both which pane types
/// go to and which region that pane belongs to.
fn pane_border(
    placement: &pane_shell::PanePlacement,
    idle: gpui::Rgba,
    opacity: f32,
) -> gpui::Rgba {
    if placement.focused { surface(placement.accent, opacity) } else { idle }
}

/// Name, count, and warn about an action that was routed but has no handler.
fn unroutable_action(action: &str, reason: &str) {
    let dropped = UNROUTABLE_ACTIONS.fetch_add(1, Ordering::Relaxed) + 1;
    tracing::warn!(action, reason, dropped, "action not wired into the GPUI shell");
}

/// Wire name of a [`ServerMessage`] variant, for the unhandled-message warning.
///
/// The match is deliberately exhaustive and deliberately not shortened with a
/// `_` arm: it is the compile-time half of the reachability gate. Adding a
/// protocol variant breaks this build until someone names it, at which point
/// `tools/check-reachability.sh` requires the variant to be either handled by
/// [`dispatch_server_message`] or recorded in the unhandled baseline.
fn server_message_variant(message: &ServerMessage) -> &'static str {
    match message {
        ServerMessage::PtyOutput { .. } => "PtyOutput",
        ServerMessage::ScreenSnapshot { .. } => "ScreenSnapshot",
        ServerMessage::SessionReplay { .. } => "SessionReplay",
        ServerMessage::AiStateChanged { .. } => "AiStateChanged",
        ServerMessage::AiStateCleared { .. } => "AiStateCleared",
        ServerMessage::CwdChanged { .. } => "CwdChanged",
        ServerMessage::SessionContextChanged { .. } => "SessionContextChanged",
        ServerMessage::TitleChanged { .. } => "TitleChanged",
        ServerMessage::CodexTaskLabelChanged { .. } => "CodexTaskLabelChanged",
        ServerMessage::CodexTaskLabelCleared { .. } => "CodexTaskLabelCleared",
        ServerMessage::TaskLabelChanged { .. } => "TaskLabelChanged",
        ServerMessage::TaskLabelCleared { .. } => "TaskLabelCleared",
        ServerMessage::PromptReceived { .. } => "PromptReceived",
        ServerMessage::WorkspaceNamed { .. } => "WorkspaceNamed",
        ServerMessage::SessionCreated { .. } => "SessionCreated",
        ServerMessage::SessionExited { .. } => "SessionExited",
        ServerMessage::Bell { .. } => "Bell",
        ServerMessage::Error { .. } => "Error",
        ServerMessage::GitBranch { .. } => "GitBranch",
        ServerMessage::SessionList { .. } => "SessionList",
        ServerMessage::WorkspaceInfo { .. } => "WorkspaceInfo",
        ServerMessage::WorkspaceNotesSnapshot { .. } => "WorkspaceNotesSnapshot",
        ServerMessage::WorkspaceNotesChanged { .. } => "WorkspaceNotesChanged",
        ServerMessage::SearchResults { .. } => "SearchResults",
        ServerMessage::Welcome { .. } => "Welcome",
        ServerMessage::WindowClosed { .. } => "WindowClosed",
        ServerMessage::WindowList { .. } => "WindowList",
        ServerMessage::RunAction { .. } => "RunAction",
        ServerMessage::ActionDispatched { .. } => "ActionDispatched",
        ServerMessage::QuitRequested => "QuitRequested",
        ServerMessage::UpdateAvailable { .. } => "UpdateAvailable",
        ServerMessage::UpdateProgress { .. } => "UpdateProgress",
        ServerMessage::UpdateCheckResult { .. } => "UpdateCheckResult",
        ServerMessage::ReleaseList { .. } => "ReleaseList",
        ServerMessage::PromptMark { .. } => "PromptMark",
        ServerMessage::TrimScrollback { .. } => "TrimScrollback",
        ServerMessage::ScrollBottom { .. } => "ScrollBottom",
        ServerMessage::EnvPreflightResult { .. } => "EnvPreflightResult",
        ServerMessage::EnvStatus { .. } => "EnvStatus",
        ServerMessage::ClipboardPromptRequest { .. } => "ClipboardPromptRequest",
        ServerMessage::ClipboardBridgeWrite { .. } => "ClipboardBridgeWrite",
        ServerMessage::ClipboardBridgeReadRequest { .. } => "ClipboardBridgeReadRequest",
        ServerMessage::RemoteHandshakeReply { .. } => "RemoteHandshakeReply",
        ServerMessage::WindowTakenOver { .. } => "WindowTakenOver",
        ServerMessage::RemoteDisconnect { .. } => "RemoteDisconnect",
        ServerMessage::RemotePeerList { .. } => "RemotePeerList",
        ServerMessage::RemoteEnv { .. } => "RemoteEnv",
        ServerMessage::LanApprovalPending => "LanApprovalPending",
        ServerMessage::LanApprovalResult { .. } => "LanApprovalResult",
        ServerMessage::LanApprovalRequest { .. } => "LanApprovalRequest",
        ServerMessage::LanPeerList { .. } => "LanPeerList",
        ServerMessage::TrustedDeviceList { .. } => "TrustedDeviceList",
        ServerMessage::TrustedNetworkList { .. } => "TrustedNetworkList",
        ServerMessage::LanEnv { .. } => "LanEnv",
        ServerMessage::LanDialIdentity { .. } => "LanDialIdentity",
        ServerMessage::ShareRoster { .. } => "ShareRoster",
        ServerMessage::ControlRequested { .. } => "ControlRequested",
        ServerMessage::ControlDenied { .. } => "ControlDenied",
        ServerMessage::ShareEnded { .. } => "ShareEnded",
    }
}

/// Drop an exited session from the registry, the AI chrome, and the tab strip.
///
/// Split out of [`dispatch_server_message`] so the dispatch table reads as one
/// screen of routing decisions rather than of session bookkeeping.
fn on_session_exited(
    ctx: &ReaderCtx,
    registry: &mut session_lifecycle::SessionRegistry,
    session_id: SessionId,
    attached: Option<SessionId>,
) -> Result<(), String> {
    let existed = registry.on_session_exited(session_id);
    update_ai_chrome(ctx, |ai| ai.forget(session_id));
    update_chrome_metadata(ctx, |metadata| metadata.forget_session(session_id));
    // The pane that showed it is retired by the view's reconcile pass; the
    // grid and the output gate are dropped here so a recycled session id can
    // never inherit the dead pane's scrollback.
    if let Ok(mut streaming) = ctx.attached.lock() {
        streaming.remove(&session_id);
    }
    if let Ok(mut grids) = ctx.panes.lock() {
        grids.forget(session_id);
    }
    let refocused = ctx.tabs.lock().ok().and_then(|mut tabs| tabs.remove(session_id));
    if let Some(next) = refocused {
        attach_session(ctx, next)?;
    }
    if existed && Some(session_id) == attached {
        set_status(&ctx.status, &ctx.generation, "attached pane exited".to_owned());
    }
    Ok(())
}

/// Repaint the attached pane from a `ScreenSnapshot` the client asked for.
///
/// The bytes are RIS followed by the snapshot's own ANSI, so the pane is
/// replaced rather than appended onto — everything on screen afterwards came
/// out of this snapshot. That is also what makes the repaint assertable from
/// outside the process, which the `RequestSnapshot` E2E relies on, so the
/// applied grid's dimensions are logged alongside the session.
fn apply_screen_snapshot(ctx: &ReaderCtx, session_id: SessionId, snapshot: &ScreenSnapshot) {
    let bytes = session_lifecycle::snapshot_reset_bytes(snapshot);
    forward_output(&ctx.in_tx, session_id, bytes);
    tracing::info!(
        %session_id,
        cols = snapshot.cols,
        rows = snapshot.rows,
        "repainted pane from server screen snapshot"
    );
}

/// Decode a reattach replay onto the pane, or surface the decode failure and
/// fall back to the per-cell snapshot path.
///
/// A corrupt reattach stream must not tear down the reader: show an error on
/// the pane and keep the connection alive. The attach still succeeded on the
/// server, so the pane is attached but showing stale content — exactly the case
/// `RequestSnapshot` exists for. Asking for the authoritative grid turns a
/// decode failure into a repaint instead of a permanently stale tab; if that
/// request cannot be enqueued the error banner is all the user gets, which is
/// the pre-existing behaviour.
fn forward_replay(ctx: &ReaderCtx, session_id: SessionId, replay: &SessionReplay) {
    match session_lifecycle::decode_replay(session_id, replay) {
        Ok(bytes) => forward_output(&ctx.in_tx, session_id, bytes),
        Err(error) => {
            set_status(&ctx.status, &ctx.generation, error.to_string());
            if let Err(sink_error) = ctx.sink.request_snapshot(session_id) {
                tracing::warn!(%sink_error, "replay-failure snapshot request dropped");
            } else {
                tracing::info!(%session_id, "requested screen snapshot after replay decode failure");
            }
        }
    }
}

async fn run_reader<R>(mut reader: R, ctx: ReaderCtx) -> Result<(), String>
where
    R: AsyncReadExt + Unpin,
{
    let mut registry = session_lifecycle::SessionRegistry::new();
    loop {
        let message: ServerMessage =
            read_message(&mut reader).await.map_err(|error| error.to_string())?;
        // The focused tab is shared state: the view moves it for `next_tab` /
        // `select_tab_N`, so output gating reads it fresh on every message
        // rather than caching a local `attached`.
        let attached = ctx.active_session.lock().ok().and_then(|guard| *guard);
        dispatch_server_message(message, &ctx, &mut registry, attached)?;
    }
}

/// Route one inbound message onto the live client state.
///
/// Every arm below is a variant this client actually implements; the trailing
/// arm hands everything else to [`unhandled_server_message`], which names the
/// variant from the exhaustive table in [`server_message_variant`] and counts
/// the drop instead of discarding it in silence. Session-scoped output arms
/// gate on `attached` inside the arm rather than in a match guard, so a frame
/// for a background pane stays a deliberate no-op instead of being reported as
/// an unhandled message. The terminal-chrome family is named here but handled
/// in [`on_chrome_message`], so this stays a table of routing decisions.
fn dispatch_server_message(
    message: ServerMessage,
    ctx: &ReaderCtx,
    registry: &mut session_lifecycle::SessionRegistry,
    attached: Option<SessionId>,
) -> Result<(), String> {
    match message {
        ServerMessage::Welcome { window_id, participant_id, .. } => {
            on_welcome(ctx, registry, window_id, participant_id);
        }
        ServerMessage::SessionList { sessions, workspaces, .. } => {
            registry.rebuild_from_session_list(&sessions);
            tracing::debug!(
                sessions = registry.len(),
                workspaces = registry.reconnect_topology().len(),
                "rebuilt reconnect topology"
            );
            // The list replays each pane's last-known CWD, branch and context,
            // so a reattach restores the status bar without waiting for the
            // next shell prompt to re-emit them.
            update_chrome_metadata(ctx, |metadata| {
                metadata.seed_from_session_list(&sessions, &workspaces);
            });
            sync_tab_strip(ctx, &sessions, attached)?;
        }
        ServerMessage::SessionCreated { session_id, workspace_id, shell_name } => {
            registry.on_session_created(session_id, workspace_id);
            open_created_tab(ctx, session_id, workspace_id, shell_name)?;
        }
        ServerMessage::SessionExited { session_id, .. } => {
            on_session_exited(ctx, registry, session_id, attached)?;
        }
        ServerMessage::AiStateChanged { session_id, ai_state } => {
            // The server has already merged partial OSC events onto the
            // stored state, so the percent that arrives here is the live one.
            update_ai_chrome(ctx, |ai| ai.tracker.update(session_id, ai_state));
        }
        ServerMessage::AiStateCleared { session_id } => {
            update_ai_chrome(ctx, |ai| {
                ai.tracker.remove(session_id);
                ai.tracker.clear_context(session_id);
            });
        }
        ServerMessage::PromptReceived { session_id, text, .. } => {
            let at = std::time::SystemTime::now();
            update_ai_chrome(ctx, |ai| ai.record_prompt(session_id, text, at));
        }
        ServerMessage::TrimScrollback { session_id, history_rows } => {
            // Track the server's scrollback trim so stored prompt marks stay
            // anchored to the right rows once command-mark tracking lands.
            let dropped = registry.on_trim_scrollback(session_id, history_rows);
            tracing::trace!(%session_id, dropped, "trimmed scrollback marks");
        }
        // The three pane-content variants are all gated on the attached pane
        // and all end in the frame queue, so they are named here and routed as
        // one to [`on_pane_output_message`].
        output @ (ServerMessage::PtyOutput { .. }
        | ServerMessage::SessionReplay { .. }
        | ServerMessage::ScreenSnapshot { .. }) => on_pane_output_message(ctx, output),
        // The terminal-chrome family all lands in the same two stores (the tab
        // strip's labels and the shared metadata), so it is named here and
        // routed as one to [`on_chrome_message`].
        chrome @ (ServerMessage::TitleChanged { .. }
        | ServerMessage::CwdChanged { .. }
        | ServerMessage::GitBranch { .. }
        | ServerMessage::SessionContextChanged { .. }
        | ServerMessage::EnvStatus { .. }
        | ServerMessage::WorkspaceNamed { .. }) => on_chrome_message(ctx, chrome),
        // The four provider task-label notices all land in the tab strip's
        // label column, so they are named here and routed as one to
        // [`on_task_label_message`]. Naming them is what keeps an AI tab's
        // label live: unnamed, they reach the drop counter below.
        label @ (ServerMessage::TaskLabelChanged { .. }
        | ServerMessage::TaskLabelCleared { .. }
        | ServerMessage::CodexTaskLabelChanged { .. }
        | ServerMessage::CodexTaskLabelCleared { .. }) => on_task_label_message(ctx, label),
        // Both update broadcasts land in the same shared state behind the
        // centred status-bar CTA, so they are named here and routed as one to
        // [`on_update_message`].
        update @ (ServerMessage::UpdateAvailable { .. } | ServerMessage::UpdateProgress { .. }) => {
            on_update_message(ctx, update);
        }
        ServerMessage::Bell { session_id } => on_bell_message(ctx, session_id),
        ServerMessage::SearchResults { session_id, query, matches } => {
            on_search_results(ctx, session_id, query, matches, attached);
        }
        ServerMessage::Error { message } => set_status(&ctx.status, &ctx.generation, message),
        // Feature 015: the four share/control notices are routed as one group so
        // this table stays a screen of routing decisions; the arm names each
        // variant, so none of them can fall through to the drop counter.
        share @ (ServerMessage::ShareRoster { .. }
        | ServerMessage::ControlRequested { .. }
        | ServerMessage::ControlDenied { .. }
        | ServerMessage::ShareEnded { .. }) => dispatch_share_message(share, ctx),
        // The three window-lifecycle answers all land in the same shared state
        // the GPUI thread's lifecycle tick drains, so they are named here and
        // routed as one to [`on_window_lifecycle_message`].
        lifecycle @ (ServerMessage::WindowClosed { .. }
        | ServerMessage::WindowList { .. }
        | ServerMessage::QuitRequested) => on_window_lifecycle_message(ctx, lifecycle),
        // Feature 014: every LAN answer — the owning side's approval push, the
        // discovery/environment replies the startup probe asks for, and the
        // connecting side's approval-gate frames — lands in the one shared
        // [`LanChrome`], so they are named here and routed as one to
        // [`on_lan_message`]. Naming them is what keeps the LAN surface
        // reachable: unnamed, an approval request reaches the drop counter and
        // the peer waits for a decision that can never be made.
        lan @ (ServerMessage::LanApprovalRequest { .. }
        | ServerMessage::LanApprovalPending
        | ServerMessage::LanApprovalResult { .. }
        | ServerMessage::LanPeerList { .. }
        | ServerMessage::LanEnv { .. }
        | ServerMessage::LanDialIdentity { .. }) => on_lan_message(ctx, lan),
        other => unhandled_server_message(&other),
    }
    Ok(())
}

/// Store one `SearchResults` reply for the find overlay and the paint path.
///
/// The reply is gated on the attached pane for the same reason `PtyOutput` is:
/// the overlay searches the pane the user is looking at, and an answer for a
/// background pane would highlight cells that pane does not own. The stored
/// reply carries its query, so the overlay can drop an answer the user has
/// already typed past instead of flashing stale matches.
fn on_search_results(
    ctx: &ReaderCtx,
    session_id: SessionId,
    query: String,
    matches: Vec<scribe_common::protocol::SearchMatch>,
    attached: Option<SessionId>,
) {
    if Some(session_id) != attached {
        tracing::debug!(%session_id, "dropped SearchResults for a background pane");
        return;
    }
    let Ok(mut results) = ctx.find.lock() else {
        tracing::warn!("find results mutex poisoned; dropping SearchResults");
        return;
    };
    tracing::info!(%session_id, %query, matches = matches.len(), "search results received");
    results.accept(query, matches);
    drop(results);
    ctx.generation.fetch_add(1, Ordering::Release);
}

/// Fold one terminal-chrome message onto the state the status bar and the tab
/// strip render from.
///
/// These six variants are the server's only channel for a pane's title,
/// working directory, git branch, shell/session context and env-capture
/// health, and for a workspace's display name. Before they were wired the
/// matching [`StatusBarData`] fields were hardcoded `None`, so every segment
/// but the connection dot and the sparklines was dead.
///
/// Anything other than the chrome family is a programming error in
/// [`dispatch_server_message`]'s routing, not a protocol event, so it is
/// counted as unhandled rather than silently dropped.
fn on_chrome_message(ctx: &ReaderCtx, message: ServerMessage) {
    match message {
        ServerMessage::TitleChanged { session_id, title } => {
            // OSC 0/2 is the pane's own label; an empty title is the shell
            // clearing it, which must not blank the tab down to nothing.
            if !title.trim().is_empty()
                && ctx.tabs.lock().is_ok_and(|mut tabs| tabs.set_title(session_id, title))
            {
                ctx.generation.fetch_add(1, Ordering::Release);
            }
        }
        ServerMessage::CwdChanged { session_id, cwd } => {
            update_chrome_metadata(ctx, |store| store.set_cwd(session_id, cwd));
        }
        ServerMessage::GitBranch { session_id, branch } => {
            update_chrome_metadata(ctx, |store| store.set_git_branch(session_id, branch));
        }
        ServerMessage::SessionContextChanged { session_id, context } => {
            update_chrome_metadata(ctx, |store| store.set_context(session_id, context));
        }
        ServerMessage::EnvStatus { session_id, state } => {
            update_chrome_metadata(ctx, |store| store.set_env_status(session_id, state));
        }
        ServerMessage::WorkspaceNamed { workspace_id, name, .. } => {
            update_chrome_metadata(ctx, |store| store.name_workspace(workspace_id, name));
        }
        // Unreachable: the caller only routes the chrome family here.
        other => unhandled_server_message(&other),
    }
}

/// Forward one pane-content message into the frame queue.
///
/// All three variants are session-scoped, so each gates on the attached set
/// inside this function rather than in a match guard: a frame for a session
/// this window is not showing stays a deliberate no-op instead of being
/// reported as an unhandled message. The gate is a set rather than the single
/// focused session because a split window streams one pane per split at once.
///
/// Anything other than the pane-content family is a programming error in
/// [`dispatch_server_message`]'s routing, not a protocol event, so it is
/// counted as unhandled rather than silently dropped.
fn on_pane_output_message(ctx: &ReaderCtx, message: ServerMessage) {
    match message {
        ServerMessage::PtyOutput { session_id, data } => {
            if is_attached(ctx, session_id) {
                // Perf gate: count drained bytes and close the echo round-trip
                // clock before the frame queue takes ownership of the payload.
                scribe_common::perf_probe::record_pty_output(data.len());
                forward_output(&ctx.in_tx, session_id, data);
            }
        }
        ServerMessage::SessionReplay { session_id, replay } => {
            if is_attached(ctx, session_id) {
                forward_replay(ctx, session_id, &replay);
            }
        }
        ServerMessage::ScreenSnapshot { session_id, snapshot } => {
            if is_attached(ctx, session_id) {
                apply_screen_snapshot(ctx, session_id, &snapshot);
            }
        }
        // Unreachable: the caller only routes the pane-content family here.
        other => unhandled_server_message(&other),
    }
}

/// Whether this window has a pane streaming `session_id`.
fn is_attached(ctx: &ReaderCtx, session_id: SessionId) -> bool {
    ctx.attached.lock().is_ok_and(|attached| attached.contains(&session_id))
}

/// Fold one provider task-label notice onto the tab strip.
///
/// While an AI tool is working on a named task the server pushes that task's
/// label as the pane's preferred tab title and clears it again when the tool
/// stops, exactly as the winit client's `handle_task_label_changed` /
/// `handle_task_label_cleared` pair did. `CodexTaskLabelChanged` /
/// `CodexTaskLabelCleared` are the pre-provider wire spelling the server still
/// emits for Codex sessions, so they carry the same meaning with the provider
/// implied.
///
/// The provider only identifies who set the label — one label per session wins,
/// so a clear from any provider drops it, matching the legacy client.
///
/// Anything other than the four label variants is a programming error in
/// [`dispatch_server_message`]'s routing, not a protocol event, so it is
/// counted as unhandled rather than silently dropped.
fn on_task_label_message(ctx: &ReaderCtx, message: ServerMessage) {
    let (session_id, provider, label) = match message {
        ServerMessage::TaskLabelChanged { session_id, provider, task_label } => {
            (session_id, provider, Some(task_label))
        }
        ServerMessage::TaskLabelCleared { session_id, provider } => (session_id, provider, None),
        ServerMessage::CodexTaskLabelChanged { session_id, task_label } => {
            (session_id, AiProvider::CodexCode, Some(task_label))
        }
        ServerMessage::CodexTaskLabelCleared { session_id } => {
            (session_id, AiProvider::CodexCode, None)
        }
        // Unreachable: the caller only routes the four label variants here.
        other => {
            unhandled_server_message(&other);
            return;
        }
    };
    let changed =
        ctx.tabs.lock().is_ok_and(|mut tabs| tabs.set_task_label(session_id, label.as_deref()));
    if changed {
        ctx.generation.fetch_add(1, Ordering::Release);
    }
    // The tab strip is pixels only, so log the transition: a scripted E2E can
    // then prove the label reached the strip and not just the socket.
    tracing::info!(
        %session_id,
        ?provider,
        label = label.as_deref().unwrap_or(""),
        changed,
        "tab task label updated"
    );
}

/// Fold one update broadcast onto the state the centred status-bar CTA renders
/// from.
///
/// These two variants are the server's only channel for update availability and
/// install progress. Before they were wired the matching [`StatusBarData`]
/// fields were hardcoded `None`, so the terminal window could neither show an
/// update nor offer one — that only worked in the `--settings` window.
fn on_update_message(ctx: &ReaderCtx, message: ServerMessage) {
    match message {
        ServerMessage::UpdateAvailable { version, release_url } => {
            tracing::info!(%version, "update available");
            update_update_state(ctx, |update| update.on_available(version, release_url));
        }
        ServerMessage::UpdateProgress { state } => {
            tracing::info!(?state, "update progress");
            update_update_state(ctx, |update| update.on_progress(state));
        }
        // Unreachable: the caller only routes the two update variants here.
        other => unhandled_server_message(&other),
    }
}

/// Queue a terminal bell for the foreground's [`BellController`] gate.
///
/// The suppression gate itself cannot run here. It is a GPUI entity, and the
/// action it authorises — a window-level attention request — belongs to the
/// thread that owns the window, so the reader records only which session belled
/// and bumps the generation so the next tick both drains the queue and repaints.
/// Recording it unconditionally is deliberate: whether this bell is suppressed
/// depends on the focus state at drain time, not at arrival time.
fn on_bell_message(ctx: &ReaderCtx, session_id: SessionId) {
    let Ok(mut queued) = ctx.bells.lock() else {
        tracing::warn!("bell queue mutex poisoned; dropping bell");
        return;
    };
    queued.push(session_id);
    drop(queued);
    tracing::info!(%session_id, "terminal bell received");
    ctx.generation.fetch_add(1, Ordering::Release);
}

/// Fold one feature-014 LAN answer onto the shared [`LanChrome`].
///
/// This is the whole LAN surface of the live reader, and it spans both roles the
/// client can be in. As the OWNING machine it receives `LanApprovalRequest` —
/// pushed by this machine's own server when an unknown device finishes the
/// mutual-TLS handshake — and parks the ported prompt for the foreground tick to
/// raise; nothing about the peer is revealed until the user answers. As the
/// CONNECTING machine it receives the approval gate's `LanApprovalPending` /
/// `LanApprovalResult` and moves the dial state, which is what turns "the window
/// is just blank" into "waiting for approval on the peer". `LanPeerList` and
/// `LanEnv` answer the startup LAN probe on either role.
///
/// Anything other than the LAN family is a programming error in
/// [`dispatch_server_message`]'s routing, not a protocol event, so it is counted
/// as unhandled rather than silently dropped.
fn on_lan_message(ctx: &ReaderCtx, message: ServerMessage) {
    match message {
        ServerMessage::LanApprovalRequest {
            request_id,
            device_name,
            fingerprint_words,
            network_label,
            name_collision,
        } => {
            // Logged because the prompt is pixels only: a scripted E2E can then
            // prove the request reached the chrome and not just the socket.
            tracing::info!(
                request_id,
                %device_name,
                %network_label,
                name_collision,
                "LAN device approval requested"
            );
            update_lan_chrome(ctx, |lan| {
                lan.park_approval(LanApprovalDialog::new(
                    request_id,
                    device_name,
                    fingerprint_words,
                    network_label,
                    name_collision,
                ));
            });
            set_status(
                &ctx.status,
                &ctx.generation,
                "a device on your local network wants to connect".to_owned(),
            );
        }
        ServerMessage::LanPeerList { peers } => {
            tracing::info!(count = peers.len(), "server LAN peer list");
            update_lan_chrome_and_status(ctx, |lan| lan.set_peers(peers));
        }
        ServerMessage::LanEnv {
            device_id_hex,
            fingerprint_words,
            current_network_addable,
            current_network_reason,
        } => {
            tracing::info!(
                identified = device_id_hex.is_some(),
                current_network_addable,
                "server LAN environment"
            );
            update_lan_chrome_and_status(ctx, |lan| {
                lan.set_env(LanEnvSummary {
                    device_id_hex,
                    fingerprint_words,
                    current_network_addable,
                    current_network_reason,
                });
            });
        }
        ServerMessage::LanApprovalPending => {
            // Normally consumed by `lan_dial::handshake` before this reader
            // exists; one arriving afterwards still means the same thing, so it
            // folds identically rather than being dropped as out of sequence.
            tracing::info!("LAN connection held pending approval on the peer");
            update_lan_chrome_and_status(ctx, LanChrome::awaiting_approval);
        }
        ServerMessage::LanApprovalResult { approved, refusal } => {
            tracing::info!(approved, ?refusal, "LAN approval result");
            let outcome = lan_approval_outcome(approved, refusal);
            update_lan_chrome_and_status(ctx, |lan| lan.settle_dial(outcome));
        }
        ServerMessage::LanDialIdentity { available, .. } => {
            // PRIVATE key material. The dialer fetches it on its own transient
            // socket before this reader exists, so one arriving on the session
            // connection is out of band: it is neither stored nor forwarded, and
            // only the presence flag is logged — never the bytes.
            tracing::warn!(
                available,
                "ignoring an out-of-band LanDialIdentity on the session connection"
            );
        }
        // Unreachable: the caller only routes the LAN family here.
        other => unhandled_server_message(&other),
    }
}

/// Map an approval gate's `approved` / `refusal` pair onto the typed dial
/// outcome. A refusal with no reason is a protocol violation, reported as a
/// generic connection failure rather than inventing a cause — the same rule
/// [`lan_dial::handshake`] applies during the preamble.
fn lan_approval_outcome(
    approved: bool,
    refusal: Option<scribe_common::protocol::LanRefusal>,
) -> LanConnectOutcome {
    match (approved, refusal) {
        (true, _) => LanConnectOutcome::Accepted,
        (false, Some(reason)) => LanConnectOutcome::Refused(reason),
        (false, None) => LanConnectOutcome::ConnectionFailure,
    }
}

/// Adopt everything the server's `Welcome` hands this connection.
///
/// The window id lands in two places on purpose: the reader's registry uses it
/// for a takeover's reattach topology, and the shell's [`WindowLifecycle`] copy
/// is what a `CloseWindow` must name — copying it here keeps the GPUI thread
/// out of the reader's registry. The additive v3 `participant_id` names this
/// connection's own share seat so a later roster matches it exactly rather than
/// by device name.
fn on_welcome(
    ctx: &ReaderCtx,
    registry: &mut session_lifecycle::SessionRegistry,
    window_id: scribe_common::ids::WindowId,
    participant_id: Option<u64>,
) {
    registry.adopt_window(window_id);
    update_lifecycle(ctx, |lifecycle| lifecycle.adopt_window(window_id));
    update_share_chrome(ctx, |share| share.set_self_id(participant_id));
    tracing::debug!(
        adopted = ?registry.adopted_window(),
        ?participant_id,
        "welcome: adopted window"
    );
}

/// Fold one window-lifecycle answer into the shared [`WindowLifecycle`].
///
/// These three variants are the server's whole side of the window's own
/// lifecycle. `QuitRequested` and a matching `WindowClosed` each arm the exit
/// the foreground's lifecycle tick performs — the reader deliberately does not
/// exit the process itself, so the window comes down through GPUI rather than
/// from an IPC thread. A `WindowClosed` naming a window this client never asked
/// to close is ignored, matching the winit client. `WindowList` refreshes the
/// status bar's owning-machine remote-control summary.
fn on_window_lifecycle_message(ctx: &ReaderCtx, message: ServerMessage) {
    match message {
        ServerMessage::QuitRequested => {
            tracing::info!("server requested quit — saving and exiting");
            update_lifecycle(ctx, WindowLifecycle::on_quit_requested);
        }
        ServerMessage::WindowClosed { window_id } => {
            update_lifecycle(ctx, |lifecycle| {
                if lifecycle.on_window_closed(window_id) {
                    tracing::info!(%window_id, "window close acknowledged by server");
                } else {
                    tracing::debug!(%window_id, "ignoring unexpected WindowClosed ack");
                }
            });
        }
        ServerMessage::WindowList { windows } => {
            log_window_list(&windows);
            update_lifecycle(ctx, |lifecycle| {
                lifecycle.set_windows(windows);
            });
        }
        // Unreachable: the caller only routes the three variants above here.
        other => unhandled_server_message(&other),
    }
}

/// Log a `WindowList` reply's shape.
///
/// The reply's only rendered consumer is a status-bar segment that shows
/// nothing at all while no window is remote-controlled, so without this line a
/// working poll and a dropped one look identical from outside the process.
fn log_window_list(windows: &[WindowInfo]) {
    let controlled = windows.iter().filter(|window| window.controller.is_some()).count();
    tracing::info!(windows = windows.len(), controlled, "server window list");
}

/// Fold one feature-015 share notice into the shared [`ShareChrome`].
///
/// A `ShareRoster` replaces the mirrored roster (and tears the surfaces down
/// once the share drains back to one participant); `ControlRequested` raises the
/// modal grant/deny prompt; `ControlDenied` and `ShareEnded` leave a transient
/// notice, the latter after clearing the share. Every arm repaints through
/// [`update_share_chrome`].
fn dispatch_share_message(message: ServerMessage, ctx: &ReaderCtx) {
    match message {
        ServerMessage::ShareRoster { window_id, participants, mode, holder } => {
            tracing::info!(%window_id, count = participants.len(), ?mode, ?holder, "share roster");
            update_share_chrome(ctx, |share| {
                share.apply_roster(ShareState { window_id, participants, mode, holder });
            });
        }
        ServerMessage::ControlRequested { window_id, from } => {
            tracing::info!(%window_id, requester = from.participant_id, "control requested");
            update_share_chrome(ctx, |share| {
                share.request(ControlRequestPrompt::new(window_id, &from));
            });
        }
        ServerMessage::ControlDenied { window_id } => {
            tracing::info!(%window_id, "control request denied");
            update_share_chrome(ctx, ShareChrome::deny);
        }
        ServerMessage::ShareEnded { window_id, reason } => {
            tracing::info!(%window_id, ?reason, "share ended");
            update_share_chrome(ctx, |share| share.end(reason));
        }
        // Unreachable: the caller only routes the four variants above here.
        other => unhandled_server_message(&other),
    }
}

/// Add a freshly created session to the tab strip, focus it, and attach.
///
/// The server also re-announces `SessionCreated` to acknowledge every
/// `AttachSessions`, so only a genuine insert counts as a new tab — attaching
/// on the echo would attach in an unbounded loop.
fn open_created_tab(
    ctx: &ReaderCtx,
    session_id: SessionId,
    workspace_id: WorkspaceId,
    shell_name: String,
) -> Result<(), String> {
    let entry = TabEntry::new(session_id, workspace_id, shell_name);
    let added = ctx.tabs.lock().is_ok_and(|mut tabs| tabs.insert_active(entry));
    if added {
        attach_session(ctx, session_id)?;
        set_status(&ctx.status, &ctx.generation, "opened a new tab".to_owned());
        // The tab strip is pixels only; log the insert so a scripted E2E can
        // assert that an action really produced a session round trip.
        tracing::info!(session = %session_id, "opened a new tab");
    }
    Ok(())
}

/// Rebuild the tab strip from an authoritative `SessionList` and attach
/// whichever tab ends up focused.
///
/// A reconnect keeps the previously focused session when it survived, so the
/// pane the user was typing into does not jump on every list refresh.
fn sync_tab_strip(
    ctx: &ReaderCtx,
    sessions: &[SessionInfo],
    attached: Option<SessionId>,
) -> Result<(), String> {
    let entries = sessions.iter().map(tab_entry_for).collect();
    let focused = ctx.tabs.lock().ok().and_then(|mut tabs| tabs.replace_all(entries));
    match focused {
        Some(session_id) if Some(session_id) != attached => {
            attach_session(ctx, session_id)?;
            set_status(
                &ctx.status,
                &ctx.generation,
                format!("attached; {} live pane(s)", sessions.len()),
            );
        }
        // Already attached to the focused tab; just repaint so any label change
        // carried by the list lands in the tab row.
        Some(_) => {
            ctx.generation.fetch_add(1, Ordering::Release);
        }
        None => set_status(
            &ctx.status,
            &ctx.generation,
            "connected; server has no live panes".to_owned(),
        ),
    }
    Ok(())
}

/// Lower a server [`SessionInfo`] into a tab strip entry.
///
/// The label prefers the session's live terminal title (OSC 0/2) and falls back
/// to the shell basename, matching what the legacy tab bar rendered. A session
/// that is mid-task when the list arrives also replays its provider task label,
/// so a reattach restores the AI tab's name instead of waiting for the provider
/// to emit the next one.
fn tab_entry_for(info: &SessionInfo) -> TabEntry {
    let mut entry = TabEntry::new(
        info.session_id,
        info.workspace_id,
        info.title.clone().unwrap_or_else(|| info.shell_name.clone()),
    );
    let label = info.task_label.as_deref().or(info.codex_task_label.as_deref());
    entry.task_label =
        label.map(str::trim).filter(|label| !label.is_empty()).map(ToOwned::to_owned);
    entry
}

/// Attach `session_id` and make it the pane the view renders and types into.
///
/// The server answers with a `SessionReplay` for the newly attached session,
/// which repaints the pane, so no extra snapshot request is needed here. The
/// trailing `Subscribe` is what registers the pane for the server's
/// CWD-fallback check, so a reconnect restores the status bar's directory and
/// workspace name without waiting for the next shell prompt; the server rejects
/// a subscription for an unattached session, which the shared ordered writer
/// channel makes impossible by construction.
fn attach_session(ctx: &ReaderCtx, session_id: SessionId) -> Result<(), String> {
    // Logged (not just surfaced in the status bar) because "the client is
    // showing a live pane" is the readiness gate the visual E2E rig waits on
    // before it drives the window; a screenshot cannot tell an unattached
    // client from an idle one.
    tracing::info!(%session_id, "attaching to session");
    ctx.out_tx
        .send(ClientMessage::AttachSessions {
            session_ids: vec![session_id],
            dimensions: vec![ctx.size],
        })
        .map_err(|_| "writer channel closed".to_owned())?;
    // Announce the client size through the sink, ahead of any KeyInput.
    ctx.sink.resize(session_id, ctx.size).map_err(|error| error.to_string())?;
    ctx.sink.subscribe(vec![session_id]).map_err(|error| error.to_string())?;
    if let Ok(mut attached) = ctx.attached.lock() {
        attached.insert(session_id);
    }
    if let Ok(mut guard) = ctx.active_session.lock() {
        *guard = Some(session_id);
    }
    Ok(())
}

fn forward_output(in_tx: &UnboundedSender<InboundEvent>, session_id: SessionId, bytes: Vec<u8>) {
    if in_tx.send(InboundEvent::PaneOutput { session_id, bytes }).is_err() {
        tracing::warn!("inbound drain closed; dropping pane output");
    }
}

fn set_status(status: &Arc<Mutex<String>>, generation: &AtomicU64, message: String) {
    if let Ok(mut status) = status.lock() {
        *status = message;
        generation.fetch_add(1, Ordering::Release);
    }
}

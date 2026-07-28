//! GPUI Scribe client over Scribe's frozen local IPC protocol.

mod ipc_bridge;
mod pane_shell;
mod session_lifecycle;
mod sync_frames;
mod terminal;
mod terminal_element;

use std::{
    collections::{HashMap, HashSet, VecDeque},
    rc::Rc,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use gpui::{
    App, AsyncApp, Bounds, Context, Entity, FocusHandle, KeyDownEvent, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, Point, Render, ScrollWheelEvent, Size, Subscription,
    Task, TitlebarOptions, WeakEntity, Window, WindowBackgroundAppearance, WindowBounds,
    WindowHandle, WindowOptions, canvas, div, prelude::*, px, relative, size,
};
use gpui_platform::application;
use scribe_client::ai_indicator::{AiStateTracker, pane_border_edges};
use scribe_client::animation::AnimationSettings;
use scribe_client::bell::{BellController, BellEvent};
use scribe_client::chrome_metadata::{ChromeMetadata, SessionChrome};
use scribe_client::clipboard::{
    self, ArboardClipboard, BridgeJob, ClipboardBackend as _, ClipboardBridge, ClipboardPrompt,
    FocusGate,
};
use scribe_client::clipboard_cleanup::{self, CopyTextOptions};
use scribe_client::color::TerminalColors as CellColors;
use scribe_client::command_palette::{
    CommandPaletteColors, CommandPaletteEvent, CommandPaletteView, PaletteAction, build_entries,
};
use scribe_client::config::{ConfigChangeSignal, ConfigReloadPlan, ConfigRuntime};
use scribe_client::context_menu::{
    ContextMenuAction, ContextMenuColors, ContextMenuEvent, ContextMenuRequest, ContextMenuView,
    MenuItem,
};
use scribe_client::dialog::{
    AnyDialog, ClipboardDialog, ClipboardDialogAction, CloseAction, CloseDialog, DialogColors,
    DialogEvent, DialogOutcome, DialogView, DisallowedSchemeAction, DisallowedSchemeDialog,
    PasteConfirmationAction, PasteConfirmationDialog, UpdateAction, UpdateDialogKind,
};
use scribe_client::divider::{self, DividerDrag};
use scribe_client::drag_drop::dropped_path_insertion;
use scribe_client::input::{self, KeyInput, TerminalMode};
use scribe_client::keybindings::{
    KeyAction, LayoutAction, OverlayChord, translate_key_action, translate_overlay_chord,
};
use scribe_client::lan::{LanChrome, LanConnectOutcome, LanEnvSummary};
use scribe_client::lan_approval::{LanApprovalAction, LanApprovalDialog};
use scribe_client::lan_dial::{self, LanDialer};
use scribe_client::layout::{FocusDirection, PaneId, Rect, SplitDirection};
use scribe_client::lost_control::{
    LostControlColors, LostControlState, ReclaimKey, lost_control_overlay,
};
use scribe_client::mouse_reporting::{self, MouseModes, ScrollDirection, WheelAction};
use scribe_client::mouse_state::{ClickKind, MouseClickState};
use scribe_client::notification_dispatcher::{self, NotifOutput, NotifReq, ShowReq};
use scribe_client::notifications::{
    AiNotice, FocusPosition, NotificationCenter, NotificationEvent, NotificationPayload,
    state_label,
};
use scribe_client::opacity::{clamp_opacity, opaque_slot, surface};
use scribe_client::paste::{PasteGate, PasteGateEvent, paste_chunks};
use scribe_client::preedit::{Ime, ImeEvent};
use scribe_client::prompt_bar::{self, PromptBarColors, PromptBarData, PromptContextIndicator};
use scribe_client::remote::{
    PeerTransport, PickerKey, RemoteConnect, RemoteConnectAction, RemoteConnectOutcome,
};
use scribe_client::remote_chrome::{RemoteChrome, RemoteEnvSummary};
use scribe_client::remote_handshake::{self, RemoteDialer};
use scribe_client::remote_picker::{RemotePickerColors, remote_picker_overlay};
use scribe_client::restore_replay::{
    self, ReplayLaunch, command_argv, prepare_replay, round_positive_f32_to_u16,
};
use scribe_client::restore_state::{AiResumeMode, LaunchBinding, RestoreStore, WindowRestoreState};
use scribe_client::scrollbar::{
    ScrollbarDrag, ScrollbarHandle, ScrollbarLayout, ScrollbarStyle, hit_test_scrollbar,
    hit_test_thumb, offset_from_drag, offset_from_track_click,
};
use scribe_client::search::{
    FindOverlayColors, FindOverlayEvent, FindOverlayView, FindResults, MatchHighlightColors,
    SEARCH_RESULT_LIMIT,
};
use scribe_client::selection::{SelectionMode, SelectionSpan};
use scribe_client::server_lifecycle;
use scribe_client::settings::{SettingsWindow, open_settings_window};
use scribe_client::share::{
    ControlIntent, ControlRequestPrompt, ShareChrome, ShareKey, ShareKeyOutcome,
    ShareOverlayColors, ShareState, share_overlay,
};
use scribe_client::smart_selection::{
    ActionExpansionContext, ResolvedSmartSelectionAction, SmartSelectionCandidate,
};
use scribe_client::split_scroll::{SplitScrollEligibility, SplitScrollState};
use scribe_client::status_bar::{self, RemoteStatusData, StatusBarColors, StatusBarData};
use scribe_client::sys_stats::SystemStatsCollector;
use scribe_client::tooltip::{TooltipColors, TooltipPosition, TooltipRender, tooltip_element};
use scribe_client::update::UpdateState;
use scribe_client::url_detect;
use scribe_client::vi_mode::ViMotion;
use scribe_client::window_chrome;
use scribe_client::window_lifecycle::{ExitReason, FocusReport, WindowLifecycle};
use scribe_client::window_state::{
    WindowGeometry, WindowRegistry, geometry_from_bounds, geometry_size_is_sane,
    normalize_legacy_geometry, window_bounds_for,
};
use scribe_client::workspace_notes::WorkspaceNotesStore;
use scribe_client::workspace_notes_modal::{
    WorkspaceNotesModalAction, WorkspaceNotesModalColors, WorkspaceNotesModalView,
};
use scribe_client::workspace_notes_preview::{
    MAX_PREVIEW_ROWS, WorkspaceNotesPreviewAction, WorkspaceNotesPreviewColors,
    WorkspaceNotesPreviewView,
};
use scribe_client::x11_focus::X11FocusGuard;
use scribe_client::zoom::ZoomState;
use scribe_client::{
    smart_selection::CompiledSmartSelection,
    tab_bar::{TabBarColors, context_suffix},
    tab_session::{TabEntry, TabSessions},
    titlebar::{TITLEBAR_HEIGHT, TitlebarEvent, TitlebarView},
};
use scribe_common::ai_state::{AiProvider, AiState};
use scribe_common::theme::ChromeColors;
use scribe_common::{
    config::{
        AiContextThresholds, SmartSelectionActionKind, SmartSelectionConfig, StatusBarStatsConfig,
        load_config,
    },
    framing::{read_message, write_message},
    ids::{SessionId, WindowId, WorkspaceId},
    protocol::{
        ArchiveReason, AutomationAction, ClientMessage, ClipboardSelection, PromptMarkKind,
        ServerMessage, SessionInfo, TerminalSize, WindowInfo,
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
    ipc_bridge::{InboundEvent, IpcSink, PaneOp, RestoredSession, run_drain},
    pane_shell::{ClosedPane, PaneShell, WorkspaceInfo, WorkspaceInfoOutcome},
    session_lifecycle::{JumpDirection, PromptMarks},
    sync_frames::{SyncFrameQueue, drain_all_committed},
    terminal::{Content, DisplayOnlyTerminal, PaneGrids, Scroll},
    terminal_element::{
        GridBounds, GridColors, GridFont, ImePaint, ScrollbarPaint, TerminalElement, cell_at,
        hits_jump_chip, record_grid_area,
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
    /// Set by the IPC reader once the server has answered this connection's
    /// first `ListSessions`. Cold-restart replay waits on it: only an *answered*
    /// and empty session list proves the server lost everything, which is the
    /// one case a persisted restore snapshot may be replayed into.
    session_list_seen: Arc<AtomicBool>,
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
    /// AI transitions the IPC reader has taken off the wire and the foreground
    /// has not yet run through the [`NotificationCenter`] gate. Queued for the
    /// same reason bells are: the decision needs the window's focus state and
    /// the live config, and the delivery needs the dispatcher handle the
    /// foreground owns.
    ai_notices: Arc<Mutex<Vec<AiNotice>>>,
    /// Sessions whose desktop notification the user clicked. Written by the
    /// dispatcher's own output thread, drained by the lifecycle tick — raising
    /// the window and selecting a tab are both foreground-only.
    notification_focus: Arc<Mutex<Vec<SessionId>>>,
    /// Latest `SearchResults` reply. The IPC reader stores it; the find
    /// overlay adopts it on the next redraw and the paint path highlights the
    /// on-screen spans it names.
    find: Arc<Mutex<FindResults>>,
    /// Feature-014 LAN state. The IPC reader parks an inbound device-approval
    /// request here and folds the peer-list, environment, and dial-gate answers
    /// into it; the view's lifecycle tick raises the parked prompt as a modal
    /// and its answer leaves through `IpcSink::lan_approval_decision`.
    lan: Arc<Mutex<LanChrome>>,
    /// Per-session OSC 133 command records. The coalescing drain anchors each
    /// `PromptMark` against the grid it has just advanced; the key path reads
    /// them back to resolve the three mark-relative jumps.
    prompt_marks: Arc<Mutex<PromptMarks>>,
    /// `WorkspaceInfo` updates the reader has taken off the wire and the GPUI
    /// thread has not yet folded onto the window's regions. The reader cannot
    /// touch them itself: a region is a GPUI entity, so the reader only parks
    /// the server's answer and the next reconcile pass applies it on the thread
    /// that owns the layout.
    workspaces: Arc<Mutex<Vec<WorkspaceInfo>>>,
    /// Server-owned workspace notes. The IPC reader folds every
    /// `WorkspaceNotesSnapshot` reply and `WorkspaceNotesChanged` broadcast into
    /// this cache; the open notes modal adopts it on the next redraw, because a
    /// modal is a GPUI entity the reader thread must not touch.
    notes: Arc<Mutex<WorkspaceNotesStore>>,
    /// Spec-010 OSC 52 state. The IPC reader records the negotiated gating bit,
    /// parks a confirmation request, and queues the host clipboard jobs the
    /// server forwards; the view's lifecycle tick raises the prompt as a modal
    /// and performs the queued jobs, because arboard and the FR-019 focus gate
    /// both belong to the thread that owns the window.
    clipboard: Arc<Mutex<ClipboardBridge>>,
    /// Feature-013 tailnet state. The IPC reader folds the peer-list,
    /// environment, dial-outcome, displacement and severance answers into it and
    /// queues inbound automation actions here; the view renders the displaced
    /// banner from it, suppresses input while that banner is up, and drains the
    /// automation queue on its lifecycle tick.
    remote: Arc<Mutex<RemoteChrome>>,
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
/// window list when it is due, raising a LAN device-approval prompt the reader
/// parked, and performing the OSC 52 clipboard work it queued.
const WINDOW_LIFECYCLE_TICK: Duration = Duration::from_millis(200);

/// How long a layout or geometry change must settle before it is written to
/// disk. A drag-resize emits a bounds change per frame and a split re-reports
/// the tree several times while sessions arrive; debouncing collapses each burst
/// into one write of the state the window actually came to rest in.
const RESTORE_DEBOUNCE: Duration = Duration::from_millis(500);

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

/// The window's overlay-scrollbar state.
///
/// Grouped rather than left as three loose view fields for the same reason
/// [`ClipboardSurfaces`] is: the drag is only ever meaningful against one of
/// the per-session records, and the palette is what every one of them paints
/// with, so all three belong to each other.
struct ScrollbarSurfaces {
    /// Per-session fade / hover / drag state. Keyed by session rather than by
    /// pane because the fade belongs to the scrollback being scrolled: moving a
    /// session between panes carries its thumb with it, and a pane with no
    /// session has nothing to scroll.
    panes: HashMap<SessionId, ScrollbarHandle>,
    /// The session whose thumb the pointer is dragging, if any. Held here
    /// rather than inferred from the pointer so a drag that wanders off the hit
    /// zone — or off the pane entirely — still resolves against the scrollbar
    /// it started on.
    drag: Option<SessionId>,
    /// Theme-derived thumb and command-tick palette, rebuilt on a theme reload.
    style: ScrollbarStyle,
}

impl ScrollbarSurfaces {
    /// Start with no panes and no drag, painting with `style`.
    fn new(style: ScrollbarStyle) -> Self {
        Self { panes: HashMap::new(), drag: None, style }
    }
}

/// Whether the pointer is currently dragging out a selection over the grid.
///
/// A two-variant enum rather than a bool because the view already carries the
/// `prompt_bar` and tooltip-demo flags, and a third loose bool is exactly the
/// shape that stops reading as a state machine.
#[derive(Clone, Copy, PartialEq, Eq)]
enum GridDrag {
    /// No button is down over the grid; pointer motion is just a hover.
    Idle,
    /// The left button is down; pointer motion extends the selection.
    Selecting,
}

/// What this process claimed out of the restore store before opening a window.
///
/// Resolved in [`main`], *before* GPUI creates the window, for two reasons. The
/// index claim is what decides how many `--restore-child` siblings this process
/// must fan out, and it has to happen once per process rather than once per
/// reconnect. And the geometry of the window being restored is only known from
/// the claimed snapshot's (pre-crash) window id, so it has to be in hand to open
/// the window at the right bounds instead of moving it a frame later.
struct ColdStart {
    /// The claimed snapshot, replayed once the server's first `SessionList`
    /// confirms it lost everything.
    snapshot: Option<WindowRestoreState>,
    /// The geometry persisted for the claimed snapshot's window, normalized and
    /// range-checked.
    geometry: Option<WindowGeometry>,
}

impl ColdStart {
    /// Claim one restore entry, fan out siblings for the rest, and load the
    /// claimed window's geometry.
    fn resolve() -> Self {
        let Some((snapshot, remaining)) = RestoreStore::new().claim_first_window() else {
            return Self { snapshot: None, geometry: None };
        };
        tracing::info!(
            window_id = %snapshot.window_id,
            remaining,
            launches = snapshot.launches.len(),
            "claimed a cold-restart snapshot"
        );
        // A fanned-out child claims exactly one entry and must never fan out
        // again: otherwise one crashed multi-window session would spawn windows
        // without bound.
        if restore_replay::is_restore_child(std::env::args()) {
            tracing::info!("restore child — not fanning out further windows");
        } else {
            restore_replay::spawn_restore_children(remaining);
        }
        // Geometry is keyed by the PRE-CRASH window id. A true cold restart
        // reaches a fresh server that has not named this window yet, and by the
        // time `Welcome` does the window is already on screen.
        let geometry = WindowRegistry::new()
            .load_saved(snapshot.window_id)
            .map(|geom| normalize_legacy_geometry(&geom))
            .filter(geometry_size_is_sane);
        Self { snapshot: Some(snapshot), geometry }
    }
}

/// The per-window inputs resolved before GPUI builds the root view.
///
/// Both are decided outside the view: the terminal size by the process-wide
/// config, the snapshot by [`ColdStart`]'s one claim against the restore index.
struct WindowSeed {
    /// Dimensions announced to the server for newly created sessions.
    terminal_size: TerminalSize,
    /// The cold-restart snapshot this window is responsible for replaying, if
    /// this process claimed one.
    restored: Option<WindowRestoreState>,
}

/// Classify what a `CreateSession` is launching so a cold restart relaunches
/// the same thing.
///
/// An AI tab is recognised from its argv rather than from the shortcut that
/// opened it, which is what lets a custom command that happens to invoke a
/// provider come back as an AI launch too. A resume invocation is matched first
/// because a resume argv also contains the provider's bare binary name.
fn launch_binding_for(command: Option<&Vec<String>>) -> LaunchBinding {
    let Some(argv) = command else {
        return restore_replay::new_shell_binding(None);
    };
    if let Some(provider) = restore_replay::detect_ai_command(argv, true) {
        return restore_replay::new_ai_binding(provider, AiResumeMode::Resume, None, None);
    }
    if let Some(provider) = restore_replay::detect_ai_command(argv, false) {
        return restore_replay::new_ai_binding(provider, AiResumeMode::New, None, None);
    }
    restore_replay::new_custom_binding(argv.clone(), None)
}

/// Everything the window needs to persist its state and replay a cold restart.
///
/// The two halves are deliberately kept together: a restore is only useful with
/// the geometry it was captured at, and both are cleared by the same explicit
/// close or quit.
struct RestoreRuntime {
    store: RestoreStore,
    registry: WindowRegistry,
    /// The snapshot [`ColdStart`] claimed, held until the first `SessionList`
    /// says whether it is needed. Taken (and dropped) either way.
    pending: Option<WindowRestoreState>,
    /// Per-session launch bindings — what a snapshot's `LaunchRecord`s are built
    /// from. A session this window did not create itself (a reattach, a share
    /// join) gets a plain shell binding, which is what a cold restart relaunches.
    bindings: HashMap<SessionId, LaunchBinding>,
    /// Bindings for sessions this window has asked for but not yet been given,
    /// oldest first. `CreateSession` carries no id, so the answering
    /// `SessionCreated` is matched by the FIFO order the single ordered writer
    /// channel guarantees — the same rule `pending_workspaces` follows.
    requested: VecDeque<LaunchBinding>,
    /// This window's server-assigned id, adopted from `Welcome`. Nothing can be
    /// persisted before it arrives: both files are keyed by it.
    window_id: Option<WindowId>,
    /// Geometry read off the live window by the bounds observer.
    geometry: Option<WindowGeometry>,
    /// The geometry already on disk, so an idle window rewrites nothing.
    saved_geometry: Option<WindowGeometry>,
    /// When the layout last changed, or `None` when the snapshot on disk is
    /// current. Drives the [`RESTORE_DEBOUNCE`] flush.
    layout_dirty_since: Option<Instant>,
    /// When the geometry last changed, on the same debounce.
    geometry_dirty_since: Option<Instant>,
    /// Set once an explicit close or quit removed the persisted state, so the
    /// tick cannot resurrect it on the way out.
    cleared: bool,
    /// True from the moment a replay dispatches its launches until every
    /// restored pane has adopted one of the answers.
    replaying: bool,
}

impl RestoreRuntime {
    fn new(pending: Option<WindowRestoreState>) -> Self {
        Self {
            store: RestoreStore::new(),
            registry: WindowRegistry::new(),
            pending,
            bindings: HashMap::new(),
            requested: VecDeque::new(),
            window_id: None,
            geometry: None,
            saved_geometry: None,
            layout_dirty_since: None,
            geometry_dirty_since: None,
            cleared: false,
            replaying: false,
        }
    }

    /// Note that the persisted snapshot no longer matches the live layout.
    fn mark_layout_dirty(&mut self) {
        if self.layout_dirty_since.is_none() {
            self.layout_dirty_since = Some(Instant::now());
        }
    }
}

/// The window's clipboard surfaces, grouped because they are always used
/// together: nothing copies without the handle, nothing pastes without the
/// gate, and no OSC 52 prompt can be answered without the request it is
/// answering.
struct ClipboardSurfaces {
    /// The live host clipboard. Owned by the view rather than the IPC reader
    /// because arboard is a window-thread resource: every copy, paste, and
    /// server-forwarded OSC 52 bridge op goes through this one handle.
    handle: ArboardClipboard,
    /// Spec-011 risky-paste gate. Every paste — chord, middle click, or context
    /// menu — is requested through it, and it decides whether the bytes go
    /// straight to the pane or park behind the confirmation modal.
    gate: Entity<PasteGate>,
    /// The OSC 52 request the open clipboard modal is answering, so its choice
    /// can be correlated back to the `request_id` the server is holding. The
    /// dialog owns its own copy for display; this is the reply copy.
    pending_prompt: Option<ClipboardPrompt>,
}

/// The window's desktop-notification surfaces, grouped because neither half is
/// useful alone: the gate decides but cannot deliver, and the dispatcher
/// delivers but knows nothing about focus, config, or which pane belongs to
/// which workspace.
struct NotificationSurfaces {
    /// The decision gate. The lifecycle tick feeds it the AI transitions the IPC
    /// reader queued plus the focus context, and it decides which of them are
    /// worth a toast.
    center: Entity<NotificationCenter>,
    /// Handle to the [`notification_dispatcher`] thread that owns the one D-Bus
    /// connection. `None` once the shutdown request has been sent, which is what
    /// makes the shutdown idempotent across the exit paths.
    tx: Option<UnboundedSender<NotifReq>>,
}

/// Pointer state behind a selection gesture: the click-count classifier that
/// picks cell / word / line granularity, and whether a drag is under way.
///
/// The two mouse-reporting fields sit beside them rather than inside `drag`, so
/// forwarding a press to a mouse-tracking application leaves the client's own
/// selection state untouched — exactly the separation the winit client kept
/// between `mouse_selecting` and `mouse_report_button`.
struct PointerState {
    clicks: MouseClickState,
    drag: GridDrag,
    /// Divider drag captured from the grid overlay.
    divider_drag: Option<DividerDrag>,
    /// The button currently forwarded to the application, or `None` when no
    /// forwarded press is outstanding. Mode 1002 gates drag motion on it, and
    /// the reported Cb carries this exact button rather than a hardcoded Left.
    report_button: Option<MouseButton>,
    /// The cell the last motion report named, for xterm's "reported only if the
    /// pointer has moved to a different character cell" de-duplication.
    report_cell: Option<(u16, u16)>,
}

/// The transient titlebar-hover notes surface and the cache version it shows.
#[derive(Default)]
struct WorkspaceNotesPreviewSurface {
    view: Option<Entity<WorkspaceNotesPreviewView>>,
    workspace_id: Option<WorkspaceId>,
    adopted: u64,
}

impl ClipboardSurfaces {
    /// Open a live host clipboard handle around an already-subscribed paste
    /// gate, with no OSC 52 prompt in flight.
    fn new(gate: Entity<PasteGate>) -> Self {
        Self { handle: ArboardClipboard::new(), gate, pending_prompt: None }
    }
}

impl Default for PointerState {
    fn default() -> Self {
        Self {
            clicks: MouseClickState::new(),
            drag: GridDrag::Idle,
            divider_drag: None,
            report_button: None,
            report_cell: None,
        }
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
    /// Overlay-scrollbar fade/hover state and the in-flight thumb drag.
    scrollbars: ScrollbarSurfaces,
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
    /// The split tree last put on the wire, so a repaint that left the topology
    /// alone does not re-report it. `None` until the first report.
    last_reported_tree: Option<scribe_common::protocol::WorkspaceTreeNode>,
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
    /// The settings window this shell opened, kept so the next settings request
    /// raises that window instead of stacking a second copy of the same
    /// surface. It is a sibling top-level window in this process, not an
    /// overlay, so it outlives any single dispatch.
    settings_window: Option<WindowHandle<SettingsWindow>>,
    /// The command palette overlay, present only while open.
    command_palette: Option<Entity<CommandPaletteView>>,
    /// The find-in-scrollback overlay, present only while open. While it is up
    /// it owns the keyboard and its match set drives the grid's highlights.
    find_overlay: Option<Entity<FindOverlayView>>,
    /// Client-local remote-connect overlay, backed by the ported picker state
    /// machine and fed from the reader-owned remote/LAN chrome snapshots.
    remote_connect: RemoteConnect,
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
    /// The hover preview anchored to the titlebar's notes affordance.
    workspace_notes_preview: WorkspaceNotesPreviewSurface,
    /// The `WorkspaceNotesStore` version the open modal is already showing, so
    /// a redraw only re-hydrates it when the reader has folded in a newer
    /// snapshot or change broadcast.
    workspace_notes_adopted: u64,
    /// OSC 8 URI held while the disallowed-scheme dialog is up, so an "Open
    /// Anyway" choice can activate the verbatim URI (spec 009 FR-015). The
    /// dialog view owns its own copy for display; this is the activation copy
    /// the shell needs after the modal resolves.
    pending_osc8_uri: Option<String>,
    /// `request_id` of the LAN device approval the open modal is answering, so
    /// the resolved choice can be correlated back to the held connection. The
    /// dialog owns its own copy for display; this is the reply copy.
    pending_lan_approval: Option<u64>,
    /// Host clipboard handle, paste gate, and the OSC 52 prompt in flight.
    clipboard: ClipboardSurfaces,
    /// Click classification and drag state behind terminal text selection.
    pointer: PointerState,
    /// Demo toggle: when set, an OSC 8-style hover tooltip is drawn over a fixed
    /// anchor so the visual E2E can exercise tooltip clamping + URL truncation.
    tooltip_demo: bool,
    /// X11 active-window guard, present only when this window has an Xcb/Xlib
    /// window id (so: X11 sessions only). Suppresses keystrokes while a
    /// compositor overlay covers the window without sending a focus event.
    x11_focus: Option<X11FocusGuard>,
    /// IME composition state, handed to the platform by the focused pane's
    /// paint pass. Owning it here (rather than per pane) matches the platform:
    /// a window has exactly one input handler, and it belongs to whichever pane
    /// currently has the keyboard.
    ime: Entity<Ime>,
    /// Terminal-bell suppression gate. The lifecycle tick feeds it the bells the
    /// IPC reader queued plus the focus context the gate reads, and the gate
    /// decides which of them are worth an attention request.
    bell: Entity<BellController>,
    /// Desktop-notification gate plus the dispatcher handle behind it.
    notifications: NotificationSurfaces,
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
    /// Cold-restart snapshot persistence, geometry persistence, and the replay
    /// of whatever this process claimed at launch.
    restore: RestoreRuntime,
    /// Held to keep the window-bounds observer alive, which is what notices a
    /// move or resize worth persisting.
    _bounds_observer: Subscription,
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
            window = scribe_client::x11_focus::xcb_window_id(window),
            "X11 active-window guard enabled"
        );
        let task = cx.spawn(async move |view, app| drive_x11_focus_polls(view, app).await);
        (guard, Some(task))
    }

    /// Veto every platform close so the in-app dialog decides what happens.
    ///
    /// The WM's close button must raise the close dialog instead of destroying
    /// the window behind the server's back: the server owns this window's
    /// sessions and has to be told whether to end them or keep them detached.
    fn register_close_veto(window: &mut Window, cx: &mut Context<Self>) {
        let close_requester = cx.weak_entity();
        window.on_window_should_close(cx, move |_window, app| {
            close_requester.update(app, TerminalView::request_window_close).unwrap_or(true)
        });
    }

    /// Watch the platform window for the moves and resizes worth persisting.
    ///
    /// This is the GPUI equivalent of the winit client's `Moved`/`Resized`
    /// handlers. It is not the only reader: the paint path takes the same
    /// reading each frame, so a window that is opened and never touched still
    /// persists the size it came up at rather than nothing at all.
    fn start_geometry_tracking(window: &mut Window, cx: &mut Context<Self>) -> Subscription {
        cx.observe_window_bounds(window, |view, window, ctx| view.capture_geometry(window, ctx))
    }

    /// Capture the live window's geometry into the restore runtime.
    ///
    /// Runs once at construction and again on every bounds change, which is the
    /// GPUI equivalent of the winit client's `Moved`/`Resized` handlers. The
    /// write itself is debounced by the lifecycle tick — a drag-resize would
    /// otherwise rewrite the file once per frame.
    fn capture_geometry(&mut self, window: &Window, cx: &App) {
        let monitor =
            window.display(cx).and_then(|display| display.uuid().ok()).map(|id| id.to_string());
        let geometry = geometry_from_bounds(window.bounds(), window.is_maximized(), monitor);
        if !geometry_size_is_sane(&geometry) || self.restore.geometry.as_ref() == Some(&geometry) {
            return;
        }
        self.restore.geometry = Some(geometry);
        if self.restore.geometry_dirty_since.is_none() {
            self.restore.geometry_dirty_since = Some(Instant::now());
        }
    }

    /// Start the three background pollers the window owns: the lifecycle tick,
    /// the redraw pump driven by the IPC drain's generation counter, and the
    /// config-reload drain.
    ///
    /// Each is held by the view, so dropping the view cancels all three.
    fn start_drivers(
        generation: Arc<AtomicU64>,
        config_signal: ConfigChangeSignal,
        cx: &mut Context<Self>,
    ) -> (Task<()>, Task<()>, Task<()>) {
        (
            cx.spawn(async move |view, app| drive_window_lifecycle(view, app).await),
            cx.spawn(async move |view, app| drive_redraws(view, app, generation).await),
            cx.spawn(async move |view, app| drive_config_reloads(view, app, config_signal).await),
        )
    }

    fn new(
        shared: Shared,
        sink: IpcSink,
        seed: WindowSeed,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let bounds_observer = Self::start_geometry_tracking(window, cx);
        let (x11_focus, x11_focus_task) = Self::start_x11_focus_guard(window, cx);
        // The focus observer is unconditional: the X11 guard is only one
        // thing an activation change drives.
        let activation_observer = cx
            .observe_window_activation(window, |view, window, ctx| view.on_activation(window, ctx));
        let (bell, bell_subscription) = Self::start_bell_gate(window, cx);
        Self::register_close_veto(window, cx);
        // Start config watching before constructing surfaces that consume it.
        let config = ConfigRuntime::start();
        let notifications = Self::start_notifications(&shared, &config, window, cx);
        let drivers = Self::start_drivers(Arc::clone(&shared.generation), config.signal(), cx);
        let (status_colors, terminal_colors, scrollbar_style) = Self::theme_palettes(&config);
        let chrome = config.config().chrome;
        let opacity = clamp_opacity(config.opacity());
        let font = GridFont::from_appearance(&config.config().config.appearance);
        let stats_config = config.config().config.terminal.status_bar_stats.clone();
        let terminal = &config.config().config.terminal;
        let smart_selection = compile_smart_selection(&terminal.smart_selection);
        let context_thresholds = terminal.ai_session.context_thresholds.clone();
        let prompt_bar_enabled = terminal.prompt_bar.enabled;
        let gate = Self::start_paste_gate(terminal.paste_confirmation, cx);
        let titlebar = Self::build_titlebar(&chrome, opacity, cx);
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
            scrollbars: ScrollbarSurfaces::new(scrollbar_style),
            grid_area: GridBounds::default(),
            published_grid_area: None,
            prompt_colors: PromptBarColors::from(&chrome),
            context_thresholds,
            prompt_bar_enabled,
            shell,
            pane_sizes: HashMap::new(),
            focused_pane_size: seed.terminal_size,
            last_reported_tree: None,
            titlebar,
            chrome,
            terminal_size: seed.terminal_size,
            // The strip starts empty and is filled by the reader's first
            // `SessionList`; `sync_tabs` pushes it into the titlebar on the
            // next redraw so the tab row always mirrors live server state.
            rendered_tabs: TabSessions::new(),
            terminal_colors,
            opacity,
            highlight_colors: MatchHighlightColors::from_chrome(&chrome),
            settings_window: None,
            command_palette: None,
            find_overlay: None,
            remote_connect: RemoteConnect::new(),
            context_menu: None,
            dialog: None,
            update_dialog_kind: None,
            workspace_notes_modal: None,
            workspace_notes_preview: WorkspaceNotesPreviewSurface::default(),
            workspace_notes_adopted: 0,
            pending_osc8_uri: None,
            pending_lan_approval: None,
            clipboard: ClipboardSurfaces::new(gate),
            pointer: PointerState::default(),
            tooltip_demo: false,
            x11_focus,
            ime: Self::start_ime(cx),
            bell,
            notifications,
            last_window_list_poll: Instant::now(),
            _refresh_task: drivers.1,
            _config_task: drivers.2,
            _x11_focus_task: x11_focus_task,
            _lifecycle_task: drivers.0,
            _activation_observer: activation_observer,
            _bell_subscription: bell_subscription,
            restore: RestoreRuntime::new(seed.restored),
            _bounds_observer: bounds_observer,
        }
    }

    /// The theme-derived palettes the window paints from: the status bar's
    /// segment colours, the terminal grid's background plus its resolved
    /// per-cell palette, and the overlay scrollbar's thumb and tick colours.
    ///
    /// Split out of [`Self::new`] so the constructor stays a list of the
    /// window's collaborators rather than also being where several of them are
    /// derived.
    fn theme_palettes(config: &ConfigRuntime) -> (StatusBarColors, GridPalette, ScrollbarStyle) {
        let theme = &config.config().theme;
        (
            StatusBarColors::from_theme(&theme.chrome, &theme.ansi_colors),
            GridPalette::from_theme(theme),
            ScrollbarStyle::from_theme(theme),
        )
    }

    /// Create the spec-011 paste gate and subscribe to its decisions.
    ///
    /// The gate is an entity so a parked paste survives the modal round trip
    /// and resumes on the exact original bytes; the view only ever sees the two
    /// outcomes it emits.
    fn start_paste_gate(confirmation_enabled: bool, cx: &mut Context<Self>) -> Entity<PasteGate> {
        let gate = cx.new(|_| PasteGate::new(confirmation_enabled));
        cx.subscribe(&gate, |view, _gate, event: &PasteGateEvent, ctx| {
            view.on_paste_gate_event(event.clone(), ctx);
        })
        .detach();
        gate
    }

    /// Create the IME composition entity and subscribe to its commits.
    ///
    /// The entity is what the focused pane hands `Window::handle_input` on
    /// every painted frame, so the OS input method (ibus/fcitx over XIM on X11,
    /// `text-input-v3` on Wayland) finally has somewhere to deliver marked and
    /// committed text — without it the platform drops both and the raw
    /// keystrokes leak straight to the PTY.
    fn start_ime(cx: &mut Context<Self>) -> Entity<Ime> {
        let entity = cx.new(|_| Ime::new());
        cx.subscribe(&entity, |view, _ime, event: &ImeEvent, ctx| {
            let ImeEvent::Commit(text) = event;
            view.commit_ime_text(text.clone(), ctx);
        })
        .detach();
        entity
    }

    /// Send composed text to the focused pane.
    ///
    /// Committed text bypasses the level-4 byte encoder deliberately: the input
    /// method already decided what characters the user meant, so it is written
    /// through the ordinary `KeyInput` path as UTF-8, exactly as the winit
    /// client's `Ime::Commit` arm did.
    fn commit_ime_text(&mut self, text: String, cx: &mut Context<Self>) {
        if text.is_empty() {
            return;
        }
        // Only the size is logged: composed text is user input from the OS
        // input method and must never reach a log line, the same rule the
        // redacted `Debug` on `PreeditState` enforces.
        tracing::info!(bytes = text.len(), "committing IME text to the focused pane");
        if self.with_focused_grid(DisplayOnlyTerminal::clear_selection) == Some(true) {
            tracing::debug!("IME commit dismissed the focused pane's selection");
        }
        self.send_key_bytes(text.into_bytes());
        cx.notify();
    }

    /// Refresh the composition anchor from the focused pane and gather what the
    /// paint pass needs to serve the input method this frame.
    ///
    /// The anchor is pushed every frame but only ever applies to the *next*
    /// composition — an in-flight one keeps the cell it started on — so a
    /// composition begun after the shell moved the cursor still lands on the
    /// right line, and one already under way stays pinned while output scrolls
    /// underneath it.
    fn sync_ime(&mut self, cx: &mut Context<Self>) -> ImePaint {
        let placement = self.with_focused_grid(|terminal| terminal.cursor_placement());
        let preedit = self.ime.update(cx, |ime, _| {
            if let Some(placement) = placement {
                ime.set_anchor(placement.abs_row, placement.col);
            }
            ime.preedit().cloned()
        });
        ImePaint {
            focus_handle: self.focus_handle.clone(),
            ime: self.ime.clone(),
            placement,
            preedit,
        }
    }

    /// Retire an in-flight composition, repainting when one was on screen.
    fn clear_preedit(&mut self, cx: &mut Context<Self>) {
        if self.ime.update(cx, |ime, _| ime.clear()) {
            tracing::info!("retired an in-flight IME composition");
            cx.notify();
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

    /// Start the desktop-notification gate and the dispatcher thread behind it.
    ///
    /// Three pieces are assembled here because they only make sense together:
    /// the [`NotificationCenter`] that decides, the
    /// [`notification_dispatcher`] thread that owns the one D-Bus connection
    /// and delivers, and the relay that turns the dispatcher's click reports
    /// back into work for the foreground.
    ///
    /// The relay is a plain thread rather than a GPUI task because the
    /// dispatcher's output channel is a tokio channel owned by a non-GPUI
    /// thread; it parks the clicked session in the shared queue the lifecycle
    /// tick already drains, which is the same hand-off the bells use.
    fn start_notifications(
        shared: &Shared,
        config: &ConfigRuntime,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> NotificationSurfaces {
        let policy = config.config().config.notifications.clone();
        let center = cx.new(|_| NotificationCenter::new(policy));
        // Detached for the same reason the titlebar's subscription is: it lives
        // exactly as long as the view, and there is nothing to unsubscribe from
        // early.
        cx.subscribe_in(&center, window, |view, _, event, window, ctx| {
            let NotificationEvent::FocusSession { session_id } = *event;
            view.focus_notified_session(session_id, window, ctx);
        })
        .detach();

        let (out_tx, mut out_rx) = unbounded_channel::<NotifOutput>();
        let clicks = Arc::clone(&shared.notification_focus);
        let relay =
            std::thread::Builder::new().name("scribe-notif-clicks".to_owned()).spawn(move || {
                while let Some(NotifOutput::FocusSession { session_id }) = out_rx.blocking_recv() {
                    tracing::info!(%session_id, "notification click asked to focus a session");
                    record_notification_click(&clicks, session_id);
                }
            });
        if let Err(error) = relay {
            tracing::warn!(%error, "could not start the notification click relay");
        }

        NotificationSurfaces { center, tx: Some(notification_dispatcher::spawn_dispatcher(out_tx)) }
    }

    /// Build the custom titlebar seeded with `tabs`, and subscribe to it.
    ///
    /// Split out of [`Self::new`] so the constructor stays a list of the
    /// window's collaborators rather than also being the place one of them is
    /// assembled.
    ///
    /// The gear button is the pointer half of the settings entry point: it has
    /// always been painted, but its [`TitlebarEvent::OpenSettings`] had no
    /// subscriber, so clicking it did nothing. It now lands on the same
    /// [`Self::open_or_focus_settings`] the chord and the palette row use.
    fn build_titlebar(
        chrome: &ChromeColors,
        opacity: f32,
        cx: &mut Context<Self>,
    ) -> Entity<TitlebarView> {
        let colors = TabBarColors::from_chrome(chrome, opacity);
        let data = TabSessions::new().to_tab_data();
        let bar = cx.new(|cx| {
            let mut bar = TitlebarView::new(colors, cx);
            bar.set_tabs(data, cx);
            bar
        });
        cx.subscribe(&bar, |this, _bar, event: &TitlebarEvent, ctx| match event {
            TitlebarEvent::OpenSettings => this.open_or_focus_settings(ctx),
            TitlebarEvent::WorkspaceNotesHover(hovered) => {
                this.set_workspace_notes_preview(*hovered, ctx);
            }
            // The tab-strip and window-control events come from the same view
            // but are their own reachability rows, outside this entry point.
            // They are named rather than folded into a `_` arm so a new
            // titlebar event fails to compile here.
            TitlebarEvent::SelectTab(_)
            | TitlebarEvent::CloseTab(_)
            | TitlebarEvent::ReorderTab { .. }
            | TitlebarEvent::WindowControl(_)
            | TitlebarEvent::Equalize => {}
        })
        .detach();
        bar
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
            self.scrollbars.style = ScrollbarStyle::from_theme(&theme);
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
        let paste_confirmation = terminal.paste_confirmation;
        self.clipboard.gate.update(cx, |gate, _| gate.set_confirmation_enabled(paste_confirmation));
        // The notification gate reads `enabled`, `condition`, and the two
        // timeout fields on every decision, so an edit to `[notifications]` has
        // to reach it or the window keeps firing on the old policy.
        let notifications = self.config.config().config.notifications.clone();
        self.notifications.center.update(cx, |center, _| center.reconfigure(notifications));

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
        self.rendered_tabs = tabs.clone();
        drop(tabs);
        let mut data = self.rendered_tabs.to_tab_data();
        let terminal = &self.config.config().config.terminal;
        let ansi = &self.config.config().theme.ansi_colors;
        if let Ok(ai) = self.shared.ai.lock() {
            for (tab, entry) in data.iter_mut().zip(self.rendered_tabs.tabs()) {
                tab.ai_indicator = ai
                    .tracker
                    .tab_indicator_color(entry.session_id, ansi, terminal)
                    .map(opaque_slot);
                tab.context_suffix =
                    tab_context_suffix_for(&ai.tracker, entry.session_id, &self.context_thresholds);
            }
        }
        self.titlebar.update(cx, |bar, ctx| {
            if bar.tabs() != data {
                bar.set_tabs(data, ctx);
            }
        });
    }

    /// Run one [`LayoutAction`] intercepted from the key path.
    ///
    /// Tab creation, selection, and closing are wired to the IPC sink, window
    /// creation opens a second top-level window, the four scrollback actions
    /// move the display viewport, the three zoom actions rescale the grid font,
    /// and the pane and workspace families drive the window's [`PaneShell`].
    /// The clipboard family serialises the focused pane's selection to the host
    /// clipboard and runs a paste back through the spec-011 gate, and the
    /// prompt-jump family moves the viewport between OSC 133 marks — so every
    /// variant now reaches a real handler.
    ///
    /// The match is exhaustive and deliberately not shortened with a `_` arm: a
    /// new [`LayoutAction`] then fails to compile here instead of silently
    /// joining a dropped set, which is the compile-time half of the
    /// reachability gate.
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
            LayoutAction::PromptJumpUp => self.jump_to_prompt(JumpDirection::Up, cx),
            LayoutAction::PromptJumpDown => self.jump_to_prompt(JumpDirection::Down, cx),
            LayoutAction::JumpToFailure => self.jump_to_failure(cx),
            LayoutAction::CopySelection => self.copy_selection(),
            LayoutAction::PasteClipboard => self.paste_clipboard(cx),
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
        // Pulse even when the viewport did not move: a page-up that hit the top
        // of scrollback is exactly when the user wants to see where they are.
        self.pulse_focused_scrollbar();
        cx.notify();
    }

    /// Move the focused pane's viewport to the neighbouring prompt mark.
    ///
    /// The marks come from the server's OSC 133 stream, anchored by the drain
    /// against the grid as it stood when each mark arrived, so a jump lands on
    /// the row the shell drew that prompt on rather than on a guess derived
    /// from the current screen.
    fn jump_to_prompt(&mut self, direction: JumpDirection, cx: &mut Context<Self>) {
        self.jump_to_mark(
            |marks, session, viewport_top_abs| {
                marks.jump_target(session, viewport_top_abs, direction)
            },
            &format!("prompt_jump_{}", if direction == JumpDirection::Up { "up" } else { "down" }),
            cx,
        );
    }

    /// Move the focused pane's viewport to the most recent failed command.
    ///
    /// A command whose shell reported no exit code stays `Unknown` and is never
    /// treated as a failure (FR-012), and when there is no failure at all the
    /// viewport is deliberately left alone (FR-011) — the jump is a navigation
    /// aid, not a mode.
    fn jump_to_failure(&mut self, cx: &mut Context<Self>) {
        self.jump_to_mark(|marks, session, _| marks.failure_target(session), "jump_to_failure", cx);
    }

    /// Shared body of the three mark-relative jumps.
    ///
    /// `pick` chooses the absolute row to land on from the focused session's
    /// marks and the viewport's current top row; everything else — resolving
    /// the focused session, dissolving the split-scroll pin the landing would
    /// otherwise outlive, moving the viewport, and logging the outcome — is
    /// identical across the three actions and lives here so they cannot drift.
    fn jump_to_mark(
        &mut self,
        pick: impl FnOnce(&PromptMarks, SessionId, usize) -> Option<usize>,
        action: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(session_id) = self.shared.active_session.lock().ok().and_then(|guard| *guard)
        else {
            return;
        };
        let Ok(marks) = self.shared.prompt_marks.lock() else {
            tracing::warn!("prompt-mark mutex poisoned; dropping jump");
            return;
        };
        let Ok(mut grids) = self.shared.panes.lock() else { return };
        let terminal = grids.grid_mut(session_id);
        let viewport_top_abs = terminal.viewport_top_abs();
        let total = marks.marks(session_id).len();
        let Some(target) = pick(&marks, session_id, viewport_top_abs) else {
            // FR-011: no candidate is a no-op, not a jump to the nearest thing.
            tracing::info!(action, marks = total, viewport_top_abs, "prompt jump found no mark");
            return;
        };
        // Landing on a mark is a deliberate scroll, so the pin is cleared for
        // the same reason `scroll_terminal` clears it on `Scroll::Bottom`.
        terminal.set_split_scroll_eligibility(SplitScrollEligibility::default());
        let moved = terminal.scroll_to_abs(target);
        let offset = terminal.display_offset();
        drop(grids);
        drop(marks);
        tracing::info!(action, target, moved, offset, marks = total, "prompt jump moved");
        // The landing row is one of the scrollbar's own ticks, so revealing the
        // overlay is what shows the jump in context: which command boundary was
        // reached, and how much scrollback sits either side of it.
        self.pulse_scrollbar(session_id);
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
    /// chord can never drift apart, and wiring one surface wires both. Every
    /// variant is named rather than folded into a `_` arm, so a new
    /// [`KeyAction`] fails to compile instead of silently joining a dropped set.
    fn dispatch_key_action(&mut self, action: KeyAction, cx: &mut Context<Self>) {
        match action {
            KeyAction::Layout(layout) => self.handle_layout_action(layout, cx),
            KeyAction::Terminal(bytes) => self.send_key_bytes(bytes),
            KeyAction::OpenCommandPalette => self.open_command_palette(cx),
            KeyAction::OpenFind => self.open_find_overlay(cx),
            KeyAction::OpenSettings => self.open_or_focus_settings(cx),
        }
    }

    /// Open the settings window from inside the running terminal window, or
    /// raise the one this shell already opened.
    ///
    /// This is the in-app twin of the `--settings` entry point: both end at
    /// [`open_settings_window`], so the chord, the palette row, and the
    /// titlebar gear all land on the same surface the CLI flag opens. GPUI is
    /// multi-window in one process, so unlike the winit client — which had to
    /// spawn the separate `scribe-settings` binary and hand focus over a Unix
    /// socket — the window is opened here and its [`WindowHandle`] retained.
    /// That handle *is* the deduplication: a second request updates it, which
    /// fails only once the window has been closed, and a live update activates
    /// the existing window instead of stacking a duplicate.
    ///
    /// The cross-process singleton ([`scribe_client::settings::singleton`])
    /// is deliberately not consulted here. It is the `--settings` launch path's
    /// guard, and its primary holds an exclusive `flock` for the whole window
    /// lifetime, so acquiring it from the terminal window would park the live
    /// shell on a lock rather than answer a keystroke.
    fn open_or_focus_settings(&mut self, cx: &mut Context<Self>) {
        if let Some(handle) = self.settings_window
            && handle.update(cx, |_, window, _| window.activate_window()).is_ok()
        {
            tracing::info!("focused the open settings window");
            return;
        }
        self.settings_window = open_settings_window(cx);
        if self.settings_window.is_some() {
            tracing::info!("opened the settings window");
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
                self.execute_automation_action(automation, ActionOrigin::Local, cx);
            }
            PaletteAction::OpenRemoteConnect => self.open_remote_connect(cx),
        }
    }

    /// Raise the client-local remote picker before requesting fresh peer lists.
    fn open_remote_connect(&mut self, cx: &mut Context<Self>) {
        self.command_palette = None;
        self.remote_connect.open();
        self.refresh_remote_peers();
        tracing::info!("opened remote-connect picker");
        cx.notify();
    }

    /// Re-request both connect-picker device lists from the local server: the
    /// 013 tailnet peers (`ListRemotePeers`) and the 014 mDNS-discovered LAN
    /// peers (`ListLanPeers`).
    ///
    /// Both replies fold into their shared chrome and mirror onto the status
    /// line, which is where the counts are visible until the picker overlay
    /// itself is ported. Ports the winit `request_remote_peers` seam exactly, so
    /// the overlay bead only has to add rendering.
    fn refresh_remote_peers(&mut self) {
        if let Err(error) = self.sink.list_remote_peers() {
            tracing::warn!(%error, "tailnet peer list request dropped: IPC writer closed");
        }
        if let Err(error) = self.sink.list_lan_peers() {
            tracing::warn!(%error, "LAN peer list request dropped: IPC writer closed");
        }
    }

    /// Copy reader-owned peer snapshots into the picker while it is still on the
    /// peer step. `RemoteConnect` ignores these updates after a target is chosen,
    /// so a late list reply cannot replace the window list the user is reading.
    fn sync_remote_connect(&mut self) {
        if !self.remote_connect.is_active() {
            return;
        }
        if let Ok(remote) = self.shared.remote.lock() {
            self.remote_connect.set_peers(remote.peers().to_vec());
        }
        if let Ok(lan) = self.shared.lan.lock() {
            self.remote_connect.set_lan_peers(lan.peers().to_vec());
        }
    }

    /// Route a GPUI keystroke into the renderer-independent picker state.
    fn handle_remote_connect_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        if !self.remote_connect.is_active() {
            return false;
        }
        let key = match event.keystroke.key.as_str() {
            "escape" => PickerKey::Escape,
            "enter" => PickerKey::Enter,
            "up" => PickerKey::Up,
            "down" => PickerKey::Down,
            "tab" => PickerKey::Tab,
            "backspace" => PickerKey::Backspace,
            "v" if event.keystroke.modifiers.control || event.keystroke.modifiers.platform => {
                PickerKey::Paste
            }
            _ => event
                .keystroke
                .key_char
                .as_ref()
                .and_then(|text| text.chars().next())
                .map_or(PickerKey::Char('\0'), PickerKey::Char),
        };
        let action = self.remote_connect.handle_key(key);
        self.apply_remote_connect_action(action, cx);
        true
    }

    /// Execute the picker intent on the foreground thread.
    fn apply_remote_connect_action(&mut self, action: RemoteConnectAction, cx: &mut Context<Self>) {
        match action {
            RemoteConnectAction::None => {}
            RemoteConnectAction::Redraw => cx.notify(),
            RemoteConnectAction::Close => {
                self.remote_connect.close();
                cx.notify();
            }
            RemoteConnectAction::RequestPeers => {
                self.refresh_remote_peers();
                cx.notify();
            }
            RemoteConnectAction::ProbeWindows { host, port, transport } => {
                Self::probe_remote_windows(host, port, transport, cx);
                cx.notify();
            }
            RemoteConnectAction::Attach { host, port, window_id, transport } => {
                spawn_remote_picker_client(transport, &host, port, Some(window_id));
                self.remote_connect.close();
                cx.notify();
            }
            RemoteConnectAction::NewWindow { host, port, transport } => {
                spawn_remote_picker_client(transport, &host, port, None);
                self.remote_connect.close();
                cx.notify();
            }
            RemoteConnectAction::PasteManual => {
                match self.clipboard.handle.read(ClipboardSelection::Clipboard) {
                    Ok(text) => self.remote_connect.append_manual(&text),
                    Err(error) => tracing::debug!(?error, "remote picker paste ignored"),
                }
                cx.notify();
            }
        }
    }

    /// Probe a selected peer before opening a remote-control child process.
    fn probe_remote_windows(
        host: String,
        port: u16,
        transport: PeerTransport,
        cx: &mut Context<Self>,
    ) {
        match transport {
            PeerTransport::Lan => Self::probe_lan_picker_windows(host, port, cx),
            PeerTransport::Tailnet => Self::probe_tailnet_picker_windows(host, port, cx),
        }
    }

    /// Bridge the LAN picker probe onto Tokio while its approval notice remains
    /// on GPUI's foreground executor.
    fn probe_lan_picker_windows(host: String, port: u16, cx: &mut Context<Self>) {
        let (pending_tx, mut pending_rx) = unbounded_channel();
        let probe = gpui_tokio::Tokio::spawn(cx, async move {
            probe_lan_picker_windows(host, port, pending_tx).await
        });
        cx.spawn(async move |view, app| {
            while pending_rx.recv().await.is_some() {
                mark_remote_picker_awaiting_approval(&view, app);
            }
        })
        .detach();
        cx.spawn(async move |view, app| {
            let probe = match probe.await {
                Ok(probe) => probe,
                Err(error) => {
                    tracing::warn!(%error, "Tokio LAN picker probe stopped unexpectedly");
                    RemotePickerProbe::LanFailure(LanConnectOutcome::ConnectionFailure)
                }
            };
            apply_remote_picker_probe(&view, app, probe);
        })
        .detach();
    }

    /// Bridge the tailnet picker probe onto Tokio, then apply its result on the
    /// GPUI-owned view.
    fn probe_tailnet_picker_windows(host: String, port: u16, cx: &mut Context<Self>) {
        let probe =
            gpui_tokio::Tokio::spawn(
                cx,
                async move { probe_tailnet_picker_windows(host, port).await },
            );
        cx.spawn(async move |view, app| {
            let probe = match probe.await {
                Ok(probe) => probe,
                Err(error) => {
                    tracing::warn!(%error, "Tokio tailnet picker probe stopped unexpectedly");
                    RemotePickerProbe::TailnetFailure(RemoteConnectOutcome::ConnectionFailure)
                }
            };
            apply_remote_picker_probe(&view, app, probe);
        })
        .detach();
    }

    /// Run one shared [`AutomationAction`].
    ///
    /// Most rows have an exact [`KeyAction`] twin, so they are lowered by
    /// [`key_action_for_automation`] and handed to [`Self::dispatch_key_action`]
    /// — the same call the keybinding path makes. The three actions with no
    /// bindable chord are handled here: a profile switch reloads the live config
    /// in place, session focus moves the tab selection, and the update row opens
    /// the same confirmation the status-bar CTA does.
    ///
    /// The one exception is a feature-015 VIEWER. Its keystrokes are already
    /// suppressed locally, and the window mutations a layout row would make —
    /// `CreateSession`, `CloseSession`, `CloseWindow` — are refused by the server
    /// for a non-controller, so running them here would fail silently. Those rows
    /// go out as [`ClientMessage::DispatchAction`] instead, which the server
    /// routes to whoever currently holds the window. Client-local rows (the find
    /// overlay, the settings window, a profile switch, tab focus) are NOT routed:
    /// they change this process, not the shared window.
    ///
    /// `origin` closes the loop that would otherwise open: an action the server
    /// delivered as a `RunAction` is already at the controller, so it is never
    /// dispatched back.
    fn execute_automation_action(
        &mut self,
        action: AutomationAction,
        origin: ActionOrigin,
        cx: &mut Context<Self>,
    ) {
        if let Some(key_action) = key_action_for_automation(&action) {
            if origin == ActionOrigin::Local
                && matches!(key_action, KeyAction::Layout(_))
                && self.share_is_viewer()
            {
                self.offer_action_to_controller(action);
                return;
            }
            self.dispatch_key_action(key_action, cx);
            return;
        }
        match action {
            AutomationAction::SwitchProfile { name } => self.switch_profile(&name, cx),
            AutomationAction::FocusSession { session_id } => self.focus_session(session_id, cx),
            AutomationAction::OpenUpdateDialog => self.open_update_dialog(cx),
            // Everything else was lowered onto a `KeyAction` above.
            other => unroutable_action(&format!("{other:?}"), "no automation handler"),
        }
    }

    /// Whether this client is a share viewer whose window mutations the server
    /// would refuse.
    fn share_is_viewer(&self) -> bool {
        self.shared.share.lock().is_ok_and(|share| share.is_viewer())
    }

    /// Ask the server to route a window-mutating action to whoever holds this
    /// window, because this client does not.
    fn offer_action_to_controller(&self, action: AutomationAction) {
        tracing::info!(?action, "routing a viewer's automation row to the control holder");
        // `None` names this connection's own window: the server refuses any
        // other id from a registered connection, and answers with the
        // `ActionDispatched` the reader logs.
        if let Err(error) = self.sink.dispatch_action(None, action) {
            tracing::warn!(%error, "automation dispatch dropped: IPC writer closed");
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
            ContextMenuAction::Copy => self.copy_selection(),
            ContextMenuAction::Paste => self.paste_clipboard(cx),
            ContextMenuAction::CopyText(text) | ContextMenuAction::CopyHyperlinkAddress(text) => {
                self.write_clipboard(text);
            }
            // Select All would have to span the whole server-owned scrollback,
            // which this display-only client does not hold; it is counted until
            // a server-side selection request exists to ask for it.
            ContextMenuAction::SelectAll => {
                unroutable_action("SelectAll", "the scrollback lives server-side");
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
    fn route_dialog_outcome(&mut self, outcome: DialogOutcome, cx: &mut Context<Self>) {
        // Four consumers read state the dialog parked on the view, so take the
        // pending URI, the pending approval id and the pending clipboard prompt
        // up front and clear the update kind only once the update route (which
        // reads it) has run.
        let pending = self.pending_osc8_uri.take();
        let approval = self.pending_lan_approval.take();
        let clipboard_prompt = self.clipboard.pending_prompt.take();
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
            // The gate still holds the exact bytes, so confirming resumes on
            // them rather than on anything re-read from the clipboard since.
            DialogOutcome::Paste(PasteConfirmationAction::Paste) => {
                self.clipboard.gate.update(cx, PasteGate::confirm);
            }
            DialogOutcome::Paste(PasteConfirmationAction::Cancel) => {
                tracing::info!("paste dropped at the confirmation");
                self.clipboard.gate.update(cx, |gate, _| gate.cancel());
            }
            DialogOutcome::Clipboard(action) => {
                self.answer_clipboard_prompt(clipboard_prompt, action);
            }
            DialogOutcome::DisallowedScheme(_) => {}
        }
        // The dialog is gone either way, so its kind must not outlive it.
        self.update_dialog_kind = None;
    }

    /// Copy the focused pane's selection to the host clipboard.
    ///
    /// Reached by the `copy` chord and by the context menu's Copy row. Nothing
    /// happens without a selection, which is what makes the chord safe to press
    /// on an empty grid: an empty copy would otherwise wipe whatever the user
    /// had on the clipboard already.
    fn copy_selection(&mut self) {
        let Some(text) = self.selection_copy_text() else {
            tracing::debug!("copy ignored: the focused pane has no selection");
            return;
        };
        self.write_clipboard(text);
    }

    /// Put `text` on the system clipboard, reporting a dead handle rather than
    /// failing silently. Shared by every copy surface.
    fn write_clipboard(&mut self, text: String) {
        let bytes = text.len();
        if let Err(error) = self.clipboard.handle.write(ClipboardSelection::Clipboard, text) {
            tracing::warn!(?error, "copy failed: the host clipboard is unavailable");
            return;
        }
        tracing::info!(bytes, "copied to the host clipboard");
    }

    /// The focused pane's selection, after the AI copy-cleanup transforms.
    ///
    /// `None` for an absent or empty selection, so every caller can treat a
    /// missing selection and an all-blank one identically.
    fn selection_copy_text(&self) -> Option<String> {
        let raw = self.with_focused_grid(|terminal| terminal.selection_text())??;
        if raw.is_empty() {
            return None;
        }
        Some(clipboard_cleanup::prepare_copy_text(&raw, self.copy_text_options()))
    }

    /// Inputs to the AI copy-cleanup transforms: the `claude_copy_cleanup`
    /// config flag plus whether the focused pane is running an AI provider at
    /// all. Copying from a plain shell pane is never rewritten.
    fn copy_text_options(&self) -> CopyTextOptions {
        let ai_session_active = self
            .shared
            .active_session
            .lock()
            .ok()
            .and_then(|guard| *guard)
            .and_then(|session_id| {
                let ai = self.shared.ai.lock().ok()?;
                ai.tracker.provider_for_session(session_id)
            })
            .is_some();
        CopyTextOptions {
            ai_session_active,
            cleanup_enabled: self.config.config().config.terminal.clipboard.claude_copy_cleanup,
        }
    }

    /// Paste the system clipboard into the focused pane (the `paste` chord and
    /// the context menu's Paste row).
    fn paste_clipboard(&mut self, cx: &mut Context<Self>) {
        match self.clipboard.handle.read(ClipboardSelection::Clipboard) {
            Ok(text) => self.request_paste(&text, cx),
            Err(error) => tracing::debug!(?error, "paste ignored: host clipboard unavailable"),
        }
    }

    /// Paste the X11 primary selection, which is what a middle click does.
    ///
    /// An empty or unavailable primary selection is skipped rather than pasted
    /// as nothing, so a stray middle click over a pane cannot deliver stale
    /// text from an unrelated app.
    fn paste_primary(&mut self, cx: &mut Context<Self>) {
        let Some(text) = clipboard::read_primary(&mut self.clipboard.handle) else {
            return;
        };
        self.request_paste(&text, cx);
    }

    /// Run `text` through the spec-011 confirmation gate on its way to the pane.
    ///
    /// The pane's bracketed-paste mode is read here rather than inside the gate
    /// because it is a property of the live `Term`: an application that opted
    /// into DEC 2004 can already tell pasted bytes from typed ones, so the gate
    /// stands down and the markers are added instead.
    fn request_paste(&mut self, text: &str, cx: &mut Context<Self>) {
        if text.is_empty() {
            return;
        }
        let bracketed =
            self.with_focused_grid(|terminal| terminal.bracketed_paste()).unwrap_or(false);
        self.clipboard.gate.update(cx, |gate, ctx| gate.request(text, bracketed, ctx));
    }

    /// Route the paste gate's decision: deliver now, or raise the confirmation.
    fn on_paste_gate_event(&mut self, event: PasteGateEvent, cx: &mut Context<Self>) {
        match event {
            PasteGateEvent::Send { text, bracketed } => {
                tracing::info!(bytes = text.len(), bracketed, "delivering a paste to the pane");
                self.deliver_paste(&text, bracketed);
            }
            PasteGateEvent::Confirm(parked) => {
                tracing::info!(bytes = parked.text.len(), "paste parked behind the confirmation");
                self.open_dialog(AnyDialog::Paste(PasteConfirmationDialog::new(parked)), cx);
            }
        }
    }

    /// Insert every file the compositor dropped on the window into the focused
    /// pane, quoted for that pane's shell.
    ///
    /// Deliberately outside the spec-011 paste confirmation gate (FR-013): the
    /// path is machine-generated and already shell-quoted, so there is nothing
    /// for a confirmation to protect against, and the winit client delivered it
    /// ungated for the same reason. Bracketed-paste markers are honoured so an
    /// editor that asked for them still sees the drop as pasted text.
    fn handle_dropped_paths(&mut self, paths: &[std::path::PathBuf], cx: &mut Context<Self>) {
        let Some(session_id) = self.shared.active_session.lock().ok().and_then(|guard| *guard)
        else {
            tracing::debug!("a file was dropped on a window with no focused pane");
            return;
        };
        let shell_name = self
            .shared
            .chrome_metadata
            .lock()
            .ok()
            .and_then(|metadata| metadata.shell_name(session_id).map(ToOwned::to_owned))
            .unwrap_or_else(|| "sh".to_owned());
        let bracketed =
            self.with_focused_grid(|terminal| terminal.bracketed_paste()).unwrap_or(false);
        for path in paths {
            let insertion = dropped_path_insertion(path, &shell_name);
            tracing::info!(
                %session_id,
                shell = %shell_name,
                bytes = insertion.len(),
                "inserting a dropped file path into the focused pane"
            );
            self.deliver_paste(&insertion, bracketed);
        }
        cx.notify();
    }

    /// Send resolved paste bytes to the focused pane, split into frames the
    /// server's `KeyInput` size limit accepts and wrapped in the DEC 2004
    /// markers when the application asked for them.
    fn deliver_paste(&self, text: &str, bracketed: bool) {
        for chunk in paste_chunks(text, bracketed) {
            self.send_key_bytes(chunk);
        }
    }

    /// Drain the OSC 52 work the IPC reader parked, on the thread that owns the
    /// window.
    ///
    /// Two jobs, both of which need the foreground: the queued host clipboard
    /// ops run here because arboard belongs to this thread and the FR-019 focus
    /// gate can only be judged against a live window, and a parked confirmation
    /// request is raised here because the modal is a GPUI entity. A modal
    /// already being up defers the prompt to a later tick instead of stacking
    /// two — the server holds the PTY-side request either way.
    fn poll_clipboard(&mut self, cx: &mut Context<Self>) {
        let (jobs, prompt) = {
            let Ok(mut bridge) = self.shared.clipboard.lock() else {
                tracing::warn!("clipboard bridge mutex poisoned; skipping the tick");
                return;
            };
            let prompt = if self.dialog.is_none() { bridge.take_prompt() } else { None };
            (bridge.drain_jobs(), prompt)
        };
        for job in jobs {
            self.run_bridge_job(job);
        }
        let Some(prompt) = prompt else {
            return;
        };
        tracing::info!(
            request_id = prompt.request_id.0,
            op = ?prompt.op,
            selection = ?prompt.selection,
            "raising the OSC 52 confirmation prompt",
        );
        let dialog = ClipboardDialog::new(
            prompt.request_id,
            prompt.op,
            prompt.selection,
            prompt.preview.clone(),
        );
        self.clipboard.pending_prompt = Some(prompt);
        self.open_dialog(AnyDialog::Clipboard(dialog), cx);
    }

    /// Perform one server-forwarded host clipboard job.
    ///
    /// A write is fire-and-forget (OSC 52 has no write ack) and a failure is
    /// silent per UX-002; a read always answers, carrying the `BridgeError` on
    /// the wire so the server can collapse it onto the empty OSC 52 reply the
    /// PTY-side program expects rather than waiting forever.
    fn run_bridge_job(&mut self, job: BridgeJob) {
        match job {
            BridgeJob::Write { selection, payload } => {
                let gate = self.write_focus_gate();
                let written =
                    clipboard::bridge_write(&mut self.clipboard.handle, selection, payload, gate);
                if let Err(error) = written {
                    tracing::debug!(?error, "OSC 52 bridge write failed");
                }
            }
            BridgeJob::Read { request_id, selection } => {
                let reply =
                    clipboard::read_reply(&mut self.clipboard.handle, request_id, selection);
                if let Err(error) = self.sink.clipboard_answer(reply) {
                    tracing::warn!(%error, "clipboard read reply dropped: IPC writer closed");
                }
            }
        }
    }

    /// The FR-019 focus-gate inputs for an OSC 52 write: the opt-in config flag
    /// and whether this window currently holds focus.
    fn write_focus_gate(&self) -> FocusGate {
        let window_focused =
            self.shared.lifecycle.lock().is_ok_and(|lifecycle| lifecycle.window_active());
        let focus_gate_writes =
            self.config.config().config.terminal.clipboard_policy.focus_gate_writes;
        FocusGate { focus_gate_writes, window_focused }
    }

    /// Answer a pending OSC 52 confirmation on the wire.
    ///
    /// The reply is not optional: the server parked the PTY-side program's
    /// request and holds it until this `request_id` resolves, so a Deny —
    /// including the Esc / backdrop default — is sent just as deliberately as
    /// an Allow. An `Always*` choice also persists the matching policy axis, so
    /// the answer outlives this session exactly as it does in the winit client.
    fn answer_clipboard_prompt(
        &mut self,
        prompt: Option<ClipboardPrompt>,
        action: ClipboardDialogAction,
    ) {
        let Some(prompt) = prompt else {
            tracing::warn!("clipboard prompt answered with no pending request");
            return;
        };
        let decision = action.decision();
        tracing::info!(
            request_id = prompt.request_id.0,
            op = ?prompt.op,
            ?decision,
            "answering the OSC 52 confirmation prompt",
        );
        clipboard::persist_policy_axis(prompt.op, decision);
        let response = clipboard::prompt_response(prompt.request_id, decision);
        if let Err(error) = self.sink.clipboard_answer(response) {
            tracing::warn!(%error, "clipboard prompt response dropped: IPC writer closed");
        }
    }

    /// Begin a selection under the pointer, at the granularity the click count
    /// names: a single click selects cells, a double selects words, and a
    /// triple or quadruple click selects whole logical lines.
    fn begin_selection(&mut self, position: Point<gpui::Pixels>, cx: &mut Context<Self>) {
        let kind = self.pointer.clicks.record_press(f32::from(position.x), f32::from(position.y));
        let Some(bounds) = self.grid_bounds.get() else {
            return;
        };
        let Some(cell) = cell_at(bounds, &self.font, position) else {
            return;
        };
        let mode = match kind {
            ClickKind::Single => SelectionMode::Cell,
            ClickKind::Double => SelectionMode::Word,
            ClickKind::Triple | ClickKind::Quadruple => SelectionMode::Line,
        };
        self.with_focused_grid(|terminal| terminal.begin_selection(cell, mode));
        self.pointer.drag = GridDrag::Selecting;
        cx.notify();
    }

    /// Extend the in-progress selection to the pointer. A no-op unless the left
    /// button is still down, so ordinary hovering costs one boolean test.
    fn extend_selection(&mut self, position: Point<gpui::Pixels>, cx: &mut Context<Self>) {
        if self.pointer.drag == GridDrag::Idle {
            return;
        }
        let Some(bounds) = self.grid_bounds.get() else {
            return;
        };
        let Some(cell) = cell_at(bounds, &self.font, position) else {
            return;
        };
        self.with_focused_grid(|terminal| terminal.extend_selection(cell));
        cx.notify();
    }

    /// Settle a selection drag: copy-on-select publishes the result to the X11
    /// primary selection so a middle click in any app pastes it, matching the
    /// winit client's `set_primary_selection`.
    fn finish_selection(&mut self, cx: &mut Context<Self>) {
        if self.pointer.drag == GridDrag::Idle {
            return;
        }
        self.pointer.drag = GridDrag::Idle;
        if !self.config.config().config.terminal.clipboard.copy_on_select {
            return;
        }
        let Some(raw) = self.with_focused_grid(|terminal| terminal.selection_text()).flatten()
        else {
            return;
        };
        let options = self.copy_text_options();
        clipboard::set_primary(&mut self.clipboard.handle, &raw, options);
        cx.notify();
    }

    /// The focused pane's selection projected onto the painted viewport.
    fn selection_spans(&self) -> Vec<SelectionSpan> {
        self.with_focused_grid(|terminal| terminal.selection_spans()).unwrap_or_default()
    }

    // -----------------------------------------------------------------------
    // Mouse reporting and the wheel
    // -----------------------------------------------------------------------

    /// The mouse-related DEC modes the focused pane's application has enabled.
    ///
    /// Defaults to "nothing enabled" with no pane attached, so every decision
    /// below falls back to the client owning the pointer.
    fn focused_mouse_modes(&self) -> MouseModes {
        self.with_focused_grid(|terminal| terminal.mouse_modes()).unwrap_or_default()
    }

    /// The grid cell under `position` in the `(col, row)` viewport coordinates
    /// a mouse report carries, or `None` when the pointer is off the grid.
    fn report_cell(&self, position: Point<gpui::Pixels>) -> Option<(u16, u16)> {
        let bounds = self.grid_bounds.get()?;
        let cell = cell_at(bounds, &self.font, position)?;
        Some((report_axis(cell.col), report_axis(cell.row)))
    }

    /// Send already-encoded application-bound bytes to the attached pane.
    ///
    /// Deliberately *not* [`Self::send_key_bytes`]: a mouse report is not a
    /// keystroke, so it must neither snap a scrolled viewport back to the live
    /// bottom nor dismiss an AI attention state. A live share viewer sends
    /// nothing at all — the server drops its input anyway, and unlike a
    /// keystroke there is no take-control affordance to raise for a mouse move.
    fn send_pty_bytes(&self, kind: &'static str, bytes: Vec<u8>) {
        if bytes.is_empty() {
            return;
        }
        if self.shared.share.lock().is_ok_and(|share| share.is_viewer()) {
            return;
        }
        let Some(session_id) = self.shared.active_session.lock().ok().and_then(|guard| *guard)
        else {
            return;
        };
        // The escaped payload is the scripted E2E's oracle: it is the only way
        // to tell a wired encoder from an unwired one without reading the wire.
        tracing::info!(kind, bytes = %escape_report_bytes(&bytes), "mouse input forwarded");
        if let Err(error) = self.sink.key_input(session_id, bytes, false) {
            tracing::warn!(%error, kind, "dropped mouse input: IPC writer closed");
        }
    }

    /// Forward a button press to a mouse-tracking application.
    ///
    /// Returns `true` when the application claimed the press, so the caller
    /// leaves the client's own gesture — selection, primary-selection paste,
    /// context menu — alone.
    fn forward_mouse_press(&mut self, event: &MouseDownEvent) -> bool {
        let modes = self.focused_mouse_modes();
        if !modes.forwards_buttons(event.modifiers.shift) {
            return false;
        }
        let Some((col, row)) = self.report_cell(event.position) else {
            return false;
        };
        let bytes = mouse_reporting::encode_mouse_press(
            event.button,
            col,
            row,
            event.modifiers,
            modes.encoding,
        );
        if bytes.is_empty() {
            return false;
        }
        self.pointer.report_button = Some(event.button);
        self.pointer.report_cell = Some((col, row));
        self.send_pty_bytes("press", bytes);
        true
    }

    /// Forward a button release to a mouse-tracking application.
    ///
    /// A physical button-up always ends the forwarded press, even when the
    /// release itself cannot be forwarded (the application turned tracking off
    /// mid-drag, or Shift is held now) — otherwise the next pointer move would
    /// report a phantom drag with a button the user is no longer holding.
    fn forward_mouse_release(&mut self, event: &MouseUpEvent) -> bool {
        let modes = self.focused_mouse_modes();
        let was_forwarded = self.pointer.report_button.take().is_some();
        self.pointer.report_cell = None;
        if !modes.forwards_buttons(event.modifiers.shift) {
            return false;
        }
        let Some((col, row)) = self.report_cell(event.position) else {
            return was_forwarded;
        };
        let bytes = mouse_reporting::encode_mouse_release(
            event.button,
            col,
            row,
            event.modifiers,
            modes.encoding,
        );
        if bytes.is_empty() {
            return was_forwarded;
        }
        self.send_pty_bytes("release", bytes);
        true
    }

    /// Forward pointer motion to a mouse-tracking application.
    ///
    /// Returns `true` whenever the application owns the pointer, including for
    /// the moves its motion level suppresses: mode 1000 reports nothing, but it
    /// still must not hand the drag back to the client's selection.
    fn forward_mouse_motion(&mut self, event: &MouseMoveEvent) -> bool {
        let modes = self.focused_mouse_modes();
        if !modes.forwards_buttons(event.modifiers.shift) {
            return false;
        }
        let Some(cell) = self.report_cell(event.position) else {
            return false;
        };
        let held = self.pointer.report_button;
        if !mouse_reporting::should_report_mouse_motion(
            modes.motion(),
            held.is_some(),
            cell,
            self.pointer.report_cell,
        ) {
            return true;
        }
        self.pointer.report_cell = Some(cell);
        let bytes = mouse_reporting::encode_mouse_motion(
            cell.0,
            cell.1,
            held,
            event.modifiers,
            modes.encoding,
        );
        self.send_pty_bytes("motion", bytes);
        true
    }

    /// Route one wheel event to whichever of the three consumers claims it.
    ///
    /// A mouse-tracking application gets a button 64 / 65 report, an alternate
    /// screen that asked for alternate scroll (1007) gets cursor keys, and
    /// anything else moves this client's own scrollback viewport.
    fn scroll_wheel(&mut self, event: &ScrollWheelEvent, cx: &mut Context<Self>) {
        let natural = self.config.config().config.terminal.scroll.natural_scroll;
        let rows = mouse_reporting::wheel_lines(event.delta, self.font.line_height, natural);
        if rows == 0 {
            return;
        }
        let modes = self.focused_mouse_modes();
        let action = mouse_reporting::wheel_action(modes);
        tracing::info!(rows, ?action, "mouse wheel");
        match action {
            WheelAction::Report => {
                let Some((col, row)) = self.report_cell(event.position) else {
                    return;
                };
                let bytes = mouse_reporting::encode_mouse_scroll(
                    ScrollDirection::from_rows(rows),
                    col,
                    row,
                    event.modifiers,
                    modes.encoding,
                );
                self.send_pty_bytes("scroll", bytes);
            }
            WheelAction::CursorKeys => {
                let bytes = mouse_reporting::alternate_scroll_keys(rows);
                self.send_pty_bytes("alternate-scroll", bytes);
            }
            WheelAction::Scrollback => self.scroll_terminal(Scroll::Delta(rows), cx),
        }
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

    /// Run every AI transition the IPC reader queued through the notification
    /// gate, on the thread that owns the window.
    ///
    /// Both of the gate's inputs live outside it and are refreshed per drain,
    /// for the same reason [`Self::poll_bells`] refreshes its own: a transition
    /// is judged against the focus state it is actually delivered under, not the
    /// one it arrived in.
    fn poll_notifications(&mut self, cx: &mut Context<Self>) {
        let notices = {
            let Ok(mut queued) = self.shared.ai_notices.lock() else {
                tracing::warn!("AI notice queue mutex poisoned; dropping queued notices");
                return;
            };
            if queued.is_empty() {
                return;
            }
            std::mem::take(&mut *queued)
        };
        let focused_session = self.shared.active_session.lock().ok().and_then(|guard| *guard);
        let window_focused =
            self.shared.lifecycle.lock().is_ok_and(|lifecycle| lifecycle.window_active());

        for notice in notices {
            match notice {
                AiNotice::StateChanged { session_id, state } => {
                    let position =
                        FocusPosition::resolve(window_focused, focused_session, session_id);
                    self.on_ai_transition(session_id, &state, position, cx);
                }
                AiNotice::Cleared { session_id } => {
                    self.notifications.center.update(cx, |center, _| center.remove(session_id));
                    self.close_notification(session_id);
                }
            }
        }
    }

    /// Judge one AI transition and fire the notification it earns.
    ///
    /// Two separate gates, in this order: the tracker decides whether the
    /// transition is notification-worthy at all (`Processing → attention`), and
    /// only then does the configured focus condition get to suppress it. The
    /// tracker still sees every transition either way, so a suppressed
    /// notification does not desynchronise the state machine.
    fn on_ai_transition(
        &mut self,
        session_id: SessionId,
        state: &AiState,
        position: FocusPosition,
        cx: &mut Context<Self>,
    ) {
        let payload = self
            .notifications
            .center
            .update(cx, |center, _| center.on_ai_state_changed(session_id, state));
        let Some(payload) = payload else { return };
        if self.notifications.center.read(cx).suppresses(position) {
            tracing::debug!(%session_id, "notification suppressed by the focus policy");
            return;
        }
        self.fire_notification(&payload, cx);
    }

    /// Hand one cleared notification decision to the dispatcher.
    ///
    /// The summary names the workspace the pane belongs to and the state it
    /// reached; the body is the pane's most recent prompt, which is what makes
    /// two toasts from different sessions tellable apart at a glance. Both come
    /// straight from the shared chrome the status bar already renders, so a
    /// notification can never describe a pane differently from the window.
    fn fire_notification(&mut self, payload: &NotificationPayload, cx: &mut Context<Self>) {
        let session_id = payload.session_id;
        let summary = format!(
            "{} — {}",
            self.notification_workspace_label(session_id),
            state_label(&payload.state)
        );
        let body = self
            .shared
            .ai
            .lock()
            .ok()
            .and_then(|ai| {
                ai.prompts.get(&session_id).and_then(|data| {
                    data.latest_prompt.clone().or_else(|| data.first_prompt.clone())
                })
            })
            .unwrap_or_default();

        // Recorded before the send: the focus-on-activate fallback is what makes
        // a click land on the right tab on platforms whose notification service
        // activates the app without naming the toast.
        self.notifications.center.update(cx, |center, _| center.set_last_notified(session_id));

        let Some(tx) = self.notifications.tx.as_ref() else {
            return;
        };
        let config = self.notifications.center.read(cx).config();
        let request = NotifReq::Show(ShowReq::new(
            session_id,
            summary.clone(),
            body,
            config.timeout_mode,
            config.timeout_secs,
        ));
        if tx.send(request).is_err() {
            tracing::debug!("notification dispatcher closed; dropping a notification");
            return;
        }
        tracing::info!(%session_id, %summary, "fired a desktop notification");
    }

    /// The label a notification names a pane by: the server's workspace name
    /// when it has one, and the app name otherwise.
    fn notification_workspace_label(&self, session_id: SessionId) -> String {
        let workspace_id = self.shared.tabs.lock().ok().and_then(|tabs| {
            tabs.tabs().iter().find(|tab| tab.session_id == session_id).map(|tab| tab.workspace_id)
        });
        workspace_id
            .and_then(|workspace_id| {
                let metadata = self.shared.chrome_metadata.lock().ok()?;
                metadata.workspace_name(workspace_id).map(ToOwned::to_owned)
            })
            .unwrap_or_else(|| scribe_common::app::current_identity().display_name().to_owned())
    }

    /// Retire the toast a session owns, if any.
    fn close_notification(&self, session_id: SessionId) {
        if let Some(tx) = self.notifications.tx.as_ref() {
            drop(tx.send(NotifReq::close(session_id)));
        }
    }

    /// Close every live toast and stop the dispatcher thread.
    fn shutdown_notifications(&mut self) {
        if let Some(tx) = self.notifications.tx.take() {
            drop(tx.send(NotifReq::Shutdown));
            tracing::info!("asked the notification dispatcher to close every live toast");
        }
    }

    /// Drain the clicks the dispatcher relay parked and act on them.
    ///
    /// Routed through the entity rather than called directly so the focus
    /// switch and the focus-on-activate fallback are consumed together: the
    /// raise this causes fires an activation, and without consuming the
    /// fallback that activation would dispatch the very same switch again.
    fn poll_notification_clicks(&mut self, cx: &mut Context<Self>) {
        let clicked = {
            let Ok(mut queue) = self.shared.notification_focus.lock() else {
                tracing::warn!("notification click queue poisoned; dropping clicks");
                return;
            };
            if queue.is_empty() {
                return;
            }
            std::mem::take(&mut *queue)
        };
        for session_id in clicked {
            self.notifications
                .center
                .update(cx, |center, ctx| center.request_focus(session_id, ctx));
        }
    }

    /// Select the clicked session's tab and raise the window.
    ///
    /// A session with no tab (it exited between the toast and the click) still
    /// raises the window: the user asked for this window, and refusing to show
    /// it would make the click look broken.
    fn focus_notified_session(
        &mut self,
        session_id: SessionId,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let index = self
            .shared
            .tabs
            .lock()
            .ok()
            .and_then(|tabs| tabs.tabs().iter().position(|tab| tab.session_id == session_id));
        if let Some(index) = index {
            self.switch_tab(move |tabs| tabs.select(index), cx);
        }
        window.activate_window();
        self.close_notification(session_id);
        tracing::info!(%session_id, "focused a session from a notification click");
        cx.notify();
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
    /// Six jobs, all of which need the foreground: the queued terminal bells
    /// are gated and signalled here (the attention request is a window call), a
    /// server-acknowledged exit can only be performed here, a focus transition
    /// the IPC reader caused (a reattach moves the focused pane with no UI event
    /// behind it) is reconciled here, the window-list poll sends from the
    /// view's own sink, the OSC 52 clipboard work the reader parked runs
    /// here because arboard and the confirmation modal both belong to this
    /// thread, and the automation actions a `RunAction` queued are executed here
    /// because the entities they drive are owned by this thread too.
    fn poll_window_lifecycle(&mut self, cx: &mut Context<Self>) {
        if let Ok(mut ai) = self.shared.ai.lock()
            && ai.tracker.clear_stale_processing()
        {
            cx.notify();
        }
        self.poll_bells(cx);
        self.poll_notifications(cx);
        self.poll_notification_clicks(cx);
        let exit = self.shared.lifecycle.lock().ok().and_then(|mut l| l.take_exit());
        if let Some(reason) = exit {
            match reason {
                ExitReason::QuitRequested => {
                    tracing::info!("quit requested by server — exiting");
                    // A quit is deliberate, so the panes must not come back —
                    // but the window's size and place should, which is why the
                    // geometry is flushed here and only the snapshot cleared.
                    self.flush_geometry_now();
                    self.clear_restore_state(false);
                }
                ExitReason::WindowClosed => {
                    tracing::info!("window close acknowledged by server — exiting");
                    self.clear_restore_state(true);
                }
            }
            // Toasts outlive the process that raised them, and a fresh client
            // cannot manage ids it never allocated — so the dispatcher closes
            // every live one before the app goes away.
            self.shutdown_notifications();
            cx.quit();
            return;
        }
        self.report_focus();
        self.poll_window_list();
        self.poll_lan_approval(cx);
        self.poll_clipboard(cx);
        self.poll_remote_actions(cx);
        self.poll_restore(cx);
    }

    /// Advance an active AI pulse on the foreground redraw clock.
    ///
    /// The clock only asks GPUI for another frame while a visible state still
    /// needs animation. Resting states remain painted but no longer keep a
    /// window-wide redraw loop alive.
    fn tick_ai_animation(&mut self, cx: &mut Context<Self>) {
        let terminal = &self.config.config().config.terminal;
        let animating = self.shared.ai.lock().is_ok_and(|mut ai| {
            if !ai.tracker.needs_animation(terminal) {
                return false;
            }
            ai.tracker.tick(0.016);
            true
        });
        if animating {
            cx.notify();
        }
    }

    // -- Cold-restart restore and window geometry persistence -----------------

    /// Run the restore machinery for one tick: adopt the window id, replay a
    /// claimed snapshot once the server has answered, and flush whichever of the
    /// two persisted files has settled.
    fn poll_restore(&mut self, cx: &mut Context<Self>) {
        if self.restore.window_id.is_none() {
            self.restore.window_id =
                self.shared.lifecycle.lock().ok().and_then(|lifecycle| lifecycle.window_id());
        }
        self.sync_launch_bindings();
        self.replay_cold_restart(cx);
        self.flush_geometry_if_due();
        if self.restore.layout_dirty_since.is_some_and(|at| at.elapsed() >= RESTORE_DEBOUNCE) {
            self.flush_snapshot_now(cx);
        }
    }

    /// Give every live session a launch binding and forget the ones that ended.
    ///
    /// A binding is what a snapshot's `LaunchRecord` is built from, so this is
    /// the thing that decides what a cold restart relaunches. Sessions this
    /// window asked for take the binding queued by the request that asked for
    /// them, in the FIFO order the one ordered writer channel guarantees;
    /// everything else (a reattach, a share join) falls back to a plain shell.
    fn sync_launch_bindings(&mut self) {
        let Ok(tabs) = self.shared.tabs.lock() else { return };
        let live: Vec<SessionId> = tabs.tabs().iter().map(|tab| tab.session_id).collect();
        drop(tabs);
        for session_id in &live {
            if self.restore.bindings.contains_key(session_id) {
                continue;
            }
            let binding = self
                .restore
                .requested
                .pop_front()
                .unwrap_or_else(|| restore_replay::new_shell_binding(None));
            self.restore.bindings.insert(*session_id, binding);
            self.restore.mark_layout_dirty();
        }
        let before = self.restore.bindings.len();
        self.restore.bindings.retain(|session_id, _| live.contains(session_id));
        if self.restore.bindings.len() != before {
            self.restore.mark_layout_dirty();
        }
    }

    /// Replay the snapshot this process claimed, if the server confirms it is
    /// needed.
    ///
    /// Three gates, in order. The server must have *answered* the startup
    /// `ListSessions` — an unanswered list is not an empty one. That answer must
    /// be empty: a server that kept this window's sessions is restored by the
    /// ordinary reattach, and replaying on top of it would double every pane.
    /// And the window must have painted once, because the restored panes are
    /// sized from the measured grid area; creating them earlier would spawn
    /// every PTY at the fallback 80x24 and leave the shell's first output
    /// formatted for a width the pane never had.
    fn replay_cold_restart(&mut self, cx: &mut Context<Self>) {
        if self.restore.pending.is_none() || !self.shared.session_list_seen.load(Ordering::Acquire)
        {
            return;
        }
        let live = self.shared.tabs.lock().map_or(0, |tabs| tabs.tabs().len());
        if live > 0 {
            tracing::info!(
                live,
                "server kept this window's sessions — dropping the cold-restart snapshot"
            );
            self.restore.pending = None;
            self.restore.mark_layout_dirty();
            return;
        }
        if self.grid_area.get().is_none() {
            return;
        }
        let Some(snapshot) = self.restore.pending.take() else { return };
        let rebuilt = prepare_replay(&snapshot);
        let bindings: HashMap<PaneId, LaunchBinding> = rebuilt
            .panes
            .iter()
            .map(|(pane_id, pane)| (*pane_id, pane.launch_binding.clone()))
            .collect();
        let launches = self.shell.adopt_restored(rebuilt, cx);
        let viewport = self.pane_viewport();
        let sizes: HashMap<PaneId, TerminalSize> = self
            .shell
            .placements(viewport, cx)
            .into_iter()
            .map(|placement| (placement.pane_id, self.grid_size_for(placement.rect)))
            .collect();
        tracing::info!(
            window_id = %snapshot.window_id,
            panes = launches.len(),
            regions = self.shell.region_count(cx),
            "replaying a cold-restart snapshot"
        );
        self.restore.replaying = !launches.is_empty();
        for launch in &launches {
            self.dispatch_replay_launch(launch, bindings.get(&launch.pane_id), &sizes);
        }
        self.report_workspace_tree(cx);
        self.restore.mark_layout_dirty();
        cx.notify();
    }

    /// Ask the server to re-create one restored pane's session.
    fn dispatch_replay_launch(
        &mut self,
        launch: &ReplayLaunch,
        binding: Option<&LaunchBinding>,
        sizes: &HashMap<PaneId, TerminalSize>,
    ) {
        // The binding is queued before the request goes out so the answering
        // `SessionCreated` re-adopts the pane's original launch id, which is
        // what keeps the next snapshot pointing at the same env envelope.
        if let Some(binding) = binding {
            self.restore.requested.push_back(binding.clone());
        }
        let size = sizes.get(&launch.pane_id).copied().unwrap_or(self.terminal_size);
        let result = self.sink.create_restored_session(RestoredSession {
            workspace_id: launch.workspace_id,
            size,
            cwd: launch.cwd.clone(),
            command: command_argv(&launch.command),
            launch_id: launch.launch_id.clone(),
        });
        match result {
            Ok(()) => tracing::info!(
                pane = launch.pane_id.raw(),
                %launch.workspace_id,
                cols = size.cols,
                rows = size.rows,
                "requested a restored session"
            ),
            Err(error) => tracing::warn!(%error, "restored session dropped: IPC writer closed"),
        }
    }

    /// Persist the window's geometry once the move or resize has settled.
    fn flush_geometry_if_due(&mut self) {
        if self.restore.geometry_dirty_since.is_some_and(|at| at.elapsed() >= RESTORE_DEBOUNCE) {
            self.flush_geometry_now();
        }
    }

    /// Write the window's geometry now, keyed by the id `Welcome` assigned.
    fn flush_geometry_now(&mut self) {
        self.restore.geometry_dirty_since = None;
        let (Some(window_id), Some(geometry)) =
            (self.restore.window_id, self.restore.geometry.clone())
        else {
            return;
        };
        if self.restore.saved_geometry.as_ref() == Some(&geometry) {
            return;
        }
        match self.restore.registry.save(window_id, &geometry) {
            Ok(()) => self.restore.saved_geometry = Some(geometry),
            Err(error) => tracing::warn!(%error, "failed to persist window geometry"),
        }
    }

    /// Write the cold-restart snapshot for this window now.
    ///
    /// A window with nothing replayable in it is *removed* from the store
    /// instead: leaving a blank entry in the index would have the next cold
    /// start claim it and replay an empty window forever.
    fn flush_snapshot_now(&mut self, cx: &mut Context<Self>) {
        self.restore.layout_dirty_since = None;
        let Some(window_id) = self.restore.window_id.filter(|_| !self.restore.cleared) else {
            return;
        };
        let snapshot = self.shell.restore_snapshot(window_id, &self.restore.bindings, cx);
        if !snapshot.is_replayable() {
            self.forget_restore_entry(window_id);
            return;
        }
        // The per-window file is written before the index entry, so a failed
        // snapshot write can never leave a dangling id in the index.
        if let Err(error) = self.restore.store.save_window(&snapshot) {
            tracing::warn!(%error, "failed to persist the cold-restart snapshot");
            return;
        }
        if let Err(error) = self.restore.store.upsert_index(window_id) {
            tracing::warn!(%error, "failed to update the restore index");
        }
    }

    /// Drop this window's snapshot and index entry.
    fn forget_restore_entry(&self, window_id: WindowId) {
        if let Err(error) = self.restore.store.remove_from_index(window_id) {
            tracing::warn!(%error, "failed to remove the window from the restore index");
        }
        self.restore.store.remove_window(window_id);
    }

    /// Clear the persisted restore state on a deliberate exit.
    ///
    /// An explicit quit or window close is the user saying these panes should
    /// not come back, so the snapshot goes; only a crash (this process dying
    /// without reaching here) leaves one behind to replay. `drop_geometry` is
    /// set for a permanent window close, where the window itself is gone and its
    /// size is no longer meaningful — a quit keeps it so the next launch reopens
    /// where the user left off.
    fn clear_restore_state(&mut self, drop_geometry: bool) {
        self.restore.cleared = true;
        self.restore.layout_dirty_since = None;
        self.restore.geometry_dirty_since = None;
        let Some(window_id) = self.restore.window_id else { return };
        self.forget_restore_entry(window_id);
        if drop_geometry {
            self.restore.registry.remove(window_id);
        }
    }

    /// Run every automation action the reader queued from a `RunAction`.
    ///
    /// The whole queue is drained per tick rather than one item per tick: two
    /// `scribe action` invocations a fifth of a second apart are a single user
    /// intent, and running the second one 200 ms late would look like a dropped
    /// command. Each action is marked [`ActionOrigin::Server`] so an action this
    /// shell cannot run is reported rather than bounced back to the server that
    /// just sent it.
    fn poll_remote_actions(&mut self, cx: &mut Context<Self>) {
        loop {
            let Some(action) = self.shared.remote.lock().ok().and_then(|mut r| r.take_action())
            else {
                return;
            };
            tracing::info!(?action, "running a server-dispatched automation action");
            self.execute_automation_action(action, ActionOrigin::Server, cx);
        }
    }

    /// Take back a window a remote controller displaced.
    ///
    /// The banner clears optimistically — matching the winit client, which drops
    /// the displaced connection and clears the state before the reclaiming
    /// `Hello` is even answered — and the claim goes out as the frozen v3
    /// [`ControlIntent::Claim`], which is how a participant takes input control
    /// of a window it is already attached to. A server that refuses the claim
    /// simply displaces this client again, which re-raises the banner.
    fn reclaim_window(&mut self, cx: &mut Context<Self>) {
        let reclaimed = self.shared.remote.lock().ok().is_some_and(|mut remote| remote.reclaim());
        if !reclaimed {
            return;
        }
        let window_id = self.shared.lifecycle.lock().ok().and_then(|l| l.window_id());
        let Some(window_id) = window_id else {
            tracing::warn!("reclaim requested before this connection adopted a window");
            cx.notify();
            return;
        };
        tracing::info!(%window_id, "reclaiming a displaced window");
        if let Err(error) = self.sink.control_intent(ControlIntent::Claim { window_id }) {
            tracing::warn!(%error, "reclaim dropped: IPC writer closed");
        }
        cx.notify();
    }

    /// The displaced banner over the frozen grid, when a remote controller holds
    /// this window.
    ///
    /// Hung last in the render tree so it covers every other overlay: while the
    /// window is frozen there is nothing else to interact with. A click anywhere
    /// on the backdrop reclaims, which is the mouse half of the one-action
    /// affordance the key path serves with Enter.
    fn build_lost_control_overlay(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let state = self.shared.remote.lock().ok()?.displaced().cloned()?;
        let colors = LostControlColors::from(&self.chrome);
        Some(
            div()
                .id("lost-control")
                .absolute()
                .inset_0()
                .on_click(cx.listener(|view, _event, _window, ctx| view.reclaim_window(ctx)))
                .child(lost_control_overlay(&state, &colors))
                .into_any_element(),
        )
    }

    /// Whether a remote controller currently holds this window.
    fn window_displaced(&self) -> bool {
        self.shared.remote.lock().is_ok_and(|remote| remote.displaced().is_some())
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
    fn create_tab(&mut self, command: Option<Vec<String>>) {
        let Some(workspace_id) = self.shared.tabs.lock().ok().and_then(|t| t.active_workspace())
        else {
            tracing::warn!("new tab ignored: no workspace is attached yet");
            return;
        };
        // Queued before the request goes out so the answering `SessionCreated`
        // is bound to what this tab actually launched — a plain shell, or the
        // AI command an AI-tab shortcut asked for, which is what a cold restart
        // has to relaunch rather than a bare login shell.
        self.restore.requested.push_back(launch_binding_for(command.as_ref()));
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
        // A deliberately opened window starts blank. Claiming a restore entry
        // here would reopen some *other* crashed window's panes inside it, which
        // is why the winit client skipped restore for `--window-id` launches.
        open_window(
            cx,
            &shared,
            &sink,
            terminal_size,
            ColdStart { snapshot: None, geometry: None },
        );
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
    /// very first frame. Running it from `render` alone would therefore never
    /// see a resize: the one repaint a bounds change buys still reads the old
    /// area, and nothing asks for another. The measuring canvas in
    /// [`Self::render_grid`] closes that gap by deferring a call back here
    /// whenever the rect it wrote actually moved, so the publish still happens
    /// on the view (never mid-paint) but always against a measured area.
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
            ClosedPane::Removed { sessions, closed_region } => {
                for session_id in &sessions {
                    self.close_pane_session(*session_id);
                }
                if let Some(workspace_id) = closed_region {
                    self.close_workspace(workspace_id);
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

    /// Tell the server a workspace region collapsed with its last pane.
    ///
    /// The shell only ever hands over regions the server itself minted, so this
    /// never names a client-local id.
    fn close_workspace(&self, workspace_id: WorkspaceId) {
        if let Err(error) = self.sink.close_workspace(workspace_id) {
            tracing::warn!(%error, "close workspace dropped: IPC writer closed");
            return;
        }
        tracing::info!(%workspace_id, "closed a workspace region on the server");
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

    /// Split the window into another workspace region, ask the server for the
    /// workspace behind it, and seed it with a session.
    ///
    /// The region is minted client-local because only the server may allocate a
    /// [`WorkspaceId`], and `CreateWorkspace` carries none: the answering
    /// `WorkspaceInfo` is what re-keys the region and hands it the server's own
    /// accent colour, one round trip later. The placeholder accent below is what
    /// the focus ring is tinted with until then.
    ///
    /// The seeded session is still created through the tab strip's workspace,
    /// because the new one does not exist yet; the `MoveSession` raised when the
    /// pane adopts it is what tells the server the session changed regions.
    fn split_workspace(&mut self, direction: SplitDirection, cx: &mut Context<Self>) {
        let accent = self.next_region_accent(cx);
        let Some(workspace_id) = self.shell.split_workspace(direction, accent, cx) else {
            tracing::warn!(?direction, "workspace split ignored: no focused region");
            return;
        };
        if let Err(error) = self.sink.create_workspace() {
            tracing::warn!(%error, "workspace creation dropped: IPC writer closed");
        }
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
    fn request_pane_session(&mut self) {
        let Some(workspace_id) = self.shared.tabs.lock().ok().and_then(|t| t.active_workspace())
        else {
            tracing::warn!("pane session ignored: no workspace is attached yet");
            return;
        };
        self.restore.requested.push_back(launch_binding_for(None));
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

    /// Republish pane geometry after the layout moved, report the new tree to
    /// the server, then repaint.
    fn after_layout_change(&mut self, cx: &mut Context<Self>) {
        self.publish_pane_sizes(cx);
        self.report_workspace_tree(cx);
        cx.notify();
    }

    /// Send the window's split tree to the server, unless it is the tree the
    /// server was last told about.
    ///
    /// The server stores the last reported tree per window and replays it on
    /// reconnect and handoff, so this is what makes a split survive a restart.
    /// It is called from every mutation path — a pane split or close, a
    /// workspace split or collapse, and the reconcile pass that fills a fresh
    /// pane with its session — which is far more often than the tree actually
    /// changes: the whole point of a pane resize or a repaint is that the
    /// topology did not move. The equality check is therefore the throttle, and
    /// it is exact rather than heuristic because the reported value *is* the
    /// wire payload.
    fn report_workspace_tree(&mut self, cx: &mut Context<Self>) {
        let tree = self.shell.wire_tree(cx);
        if self.last_reported_tree.as_ref() == Some(&tree) {
            return;
        }
        if let Err(error) = self.sink.report_workspace_tree(tree.clone()) {
            tracing::warn!(%error, "workspace tree report dropped: IPC writer closed");
            return;
        }
        self.last_reported_tree = Some(tree);
        // Every layout change funnels through here, so this is also where the
        // cold-restart snapshot learns it is stale — the same trigger the winit
        // client debounced its own save on.
        self.restore.mark_layout_dirty();
        tracing::info!(
            regions = self.shell.region_count(cx),
            panes = self.shell.pane_count(cx),
            "reported the workspace tree to the server"
        );
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

    /// The terminal grid a pane rect resolves to at the live font metrics.
    ///
    /// Shared by the per-frame republish and the cold-restart replay so a
    /// restored PTY is created at exactly the size the pane it lands in will
    /// report one frame later.
    fn grid_size_for(&self, rect: Rect) -> TerminalSize {
        let cell_width = self.font.cell_width();
        let line_height = self.font.line_height;
        TerminalSize {
            cols: round_positive_f32_to_u16((rect.width / cell_width).floor()).max(1),
            rows: round_positive_f32_to_u16((rect.height / line_height).floor()).max(1),
            cell_width: round_positive_f32_to_u16(cell_width).max(1),
            cell_height: round_positive_f32_to_u16(line_height).max(1),
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
        let placements = self.shell.placements(viewport, cx);
        let live: HashSet<SessionId> =
            placements.iter().filter_map(|placement| placement.session_id).collect();
        self.pane_sizes.retain(|session, _| live.contains(session));
        for placement in placements {
            let size = self.grid_size_for(placement.rect);
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
        // Ahead of everything else: a `WorkspaceInfo` is the answer to the
        // `CreateWorkspace` a split sent, and the session that split asked for
        // is adopted below. Applying it first is what lets the adoption see the
        // region's real workspace and raise a `MoveSession` for it.
        let mut changed = self.adopt_workspace_info(cx);
        if let Some(workspace_id) = self.shared.tabs.lock().ok().and_then(|t| t.active_workspace())
        {
            changed |= self.shell.adopt_server_workspace(workspace_id, cx);
        }
        let live: HashSet<SessionId> = self.shared.tabs.lock().map_or_else(
            |_| HashSet::new(),
            |tabs| tabs.tabs().iter().map(|tab| tab.session_id).collect(),
        );
        if !live.is_empty() {
            self.retire_scrollbars(&live);
            let retired = self.shell.retain_sessions(&live, cx);
            for workspace_id in retired.closed_regions {
                self.close_workspace(workspace_id);
            }
            changed |= retired.changed;
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
                self.follow_session_to_region(pane, session_id, cx);
                changed = true;
            }
        }
        changed |= self.fill_pending_panes(cx);
        if changed {
            self.publish_pane_sizes(cx);
            self.report_workspace_tree(cx);
        }
    }

    /// Hand every still-queued pane one of the sessions that has arrived but is
    /// not on screen yet.
    ///
    /// A pane is only pending because something explicitly asked the server for
    /// a session to put in it — a split, or a cold-restart replay. The
    /// active-session path above adopts one such answer per pass, which is
    /// enough for a split but not for a replay: five `CreateSession` frames come
    /// back faster than five ticks, so four of the five panes would stay empty
    /// and their sessions would live on as tabs with nowhere to render.
    /// Gated on a replay being in flight, because outside one the pairing would
    /// be wrong: a split's pending pane must get the session that split asked
    /// for, not whichever older tab happens to have no pane. A replay starts
    /// from a server with nothing, so every unshown session it sees is one of
    /// its own answers.
    fn fill_pending_panes(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.restore.replaying {
            return false;
        }
        let mut changed = false;
        loop {
            let shown = self.shell.shown_sessions();
            let unshown = self.shared.tabs.lock().ok().and_then(|tabs| {
                tabs.tabs().iter().map(|tab| tab.session_id).find(|id| !shown.contains(id))
            });
            let Some(session_id) = unshown else { break };
            let Some(pane) = self.shell.take_pending(cx) else { break };
            self.adopt_session(pane, session_id);
            self.follow_session_to_region(pane, session_id, cx);
            changed = true;
        }
        if !self.shell.has_pending() {
            tracing::info!("cold-restart replay filled every restored pane");
            self.restore.replaying = false;
        }
        changed
    }

    /// Fold every parked `WorkspaceInfo` onto the region it names.
    ///
    /// Returns whether the layout changed, which it does when a region that was
    /// waiting for a server workspace adopted one — a rename that moves the
    /// region's id and therefore the workspace every later frame reports.
    fn adopt_workspace_info(&mut self, cx: &mut Context<Self>) -> bool {
        let Ok(mut guard) = self.shared.workspaces.lock() else {
            tracing::warn!("workspace info mutex poisoned; skipping this pass");
            return false;
        };
        let parked: Vec<WorkspaceInfo> = std::mem::take(&mut *guard);
        drop(guard);
        let mut changed = false;
        for info in parked {
            let workspace_id = info.workspace_id;
            match self.shell.apply_workspace_info(&info, cx) {
                WorkspaceInfoOutcome::Adopted => {
                    tracing::info!(%workspace_id, "a workspace region adopted a server workspace");
                    changed = true;
                }
                WorkspaceInfoOutcome::Updated => {
                    tracing::debug!(%workspace_id, "refreshed a workspace region's metadata");
                    changed = true;
                }
                WorkspaceInfoOutcome::Unclaimed => {
                    tracing::debug!(%workspace_id, "no region claimed this workspace");
                }
            }
        }
        changed
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

    /// Tell the server a session changed workspace regions.
    ///
    /// A pane and a workspace move on different axes: a `workspace_split_*`
    /// opens a region *before* the server has minted its workspace, so the
    /// session it seeds is necessarily created through the workspace the tab
    /// strip was pointing at. Once the pane that adopts it turns out to sit in
    /// a different region, this is the frame that reconciles the two — without
    /// it the server keeps every session filed under the window's first
    /// workspace no matter which region the user put it in.
    ///
    /// Only regions the server minted are named, and the strip is re-filed
    /// optimistically so a later split seeds its session in the region the user
    /// is actually looking at.
    fn follow_session_to_region(
        &mut self,
        pane: PaneId,
        session_id: SessionId,
        cx: &mut Context<Self>,
    ) {
        let Some(target) = self.shell.region_for_pane(pane, cx) else { return };
        if !self.shell.is_server_workspace(target) {
            return;
        }
        let Ok(mut tabs) = self.shared.tabs.lock() else { return };
        if !tabs.set_workspace(session_id, target) {
            return;
        }
        drop(tabs);
        if let Err(error) = self.sink.move_session(session_id, target) {
            tracing::warn!(%error, "session move dropped: IPC writer closed");
            return;
        }
        tracing::info!(%session_id, %target, "moved a session into another workspace region");
    }

    /// The last painted frame of `session_id`'s grid, or `None` when nothing
    /// has ever reached that pane.
    fn pane_content(&self, session_id: SessionId) -> Option<Content> {
        self.shared.panes.lock().ok()?.content(session_id)
    }

    /// Resolve one animated AI border colour per workspace for this frame.
    fn workspace_ai_borders(
        &self,
        placements: &[pane_shell::PanePlacement],
    ) -> HashMap<WorkspaceId, gpui::Rgba> {
        let mut sessions: HashMap<WorkspaceId, Vec<SessionId>> = HashMap::new();
        for placement in placements {
            if let Some(session_id) = placement.session_id {
                sessions.entry(placement.workspace_id).or_default().push(session_id);
            }
        }
        let terminal = &self.config.config().config.terminal;
        let ansi = &self.config.config().theme.ansi_colors;
        self.shared.ai.lock().ok().map_or_else(HashMap::new, |ai| {
            sessions
                .iter()
                .filter_map(|(workspace_id, sessions)| {
                    ai.tracker
                        .workspace_border_color(sessions, ansi, terminal)
                        .map(|color| (*workspace_id, opaque_slot(color)))
                })
                .collect()
        })
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
    fn render_panes(&mut self, focused: Content, ime: ImePaint, cx: &App) -> Vec<gpui::AnyElement> {
        let viewport = self.pane_viewport();
        if viewport.width <= 0.0 || viewport.height <= 0.0 {
            return Vec::new();
        }
        let placements = self.shell.placements(viewport, cx);
        let workspace_ai_borders = self.workspace_ai_borders(&placements);
        // Mint any missing scrollbar state before the render closure below
        // borrows `self` immutably. The state has to outlive the element (the
        // fade is a wall-clock animation across frames), so it lives here and
        // the element only borrows a handle to it.
        for session_id in placements.iter().filter_map(|placement| placement.session_id) {
            self.scrollbars.panes.entry(session_id).or_default();
        }
        let split = placements.len() > 1;
        let selection_spans = self.selection_spans();
        let mut ime = Some(ime);
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
                let ai_border = workspace_ai_borders.get(&placement.workspace_id).copied();
                // Only the focused pane publishes its painted bounds: they are
                // what `cell_at` resolves a pointer against, and the mouse
                // path acts on the focused pane.
                let (highlights, bounds) = if placement.focused {
                    (self.find_highlights(&content, cx), Rc::clone(&self.grid_bounds))
                } else {
                    (Vec::new(), GridBounds::default())
                };
                // Selection lives on the focused pane only, for the same reason
                // the bounds do: it is driven by a pointer this window resolves
                // against that one pane.
                let selection =
                    if placement.focused { selection_spans.clone() } else { Vec::new() };
                let mut element = TerminalElement::new(
                    content,
                    self.font.clone(),
                    colors,
                    self.highlight_colors,
                    bounds,
                )
                .with_highlights(highlights)
                .with_selection(selection);
                // Every pane showing a session gets its own scrollbar: each
                // pane scrolls its own scrollback, so unlike the IME handler
                // this is not a window-wide singleton.
                if let Some(scrollbar) =
                    placement.session_id.and_then(|session| self.scrollbar_paint(session))
                {
                    element = element.with_scrollbar(scrollbar);
                }
                // The input handler is a window-level singleton, so it is
                // registered by the focused pane alone — `take` guarantees that
                // even if the layout ever reported two focused placements.
                if placement.focused
                    && let Some(ime) = ime.take()
                {
                    element = element.with_ime(ime);
                }
                let mut pane = pane.child(element.paint());
                if let Some(color) = ai_border {
                    pane = pane.children(ai_pane_border(placement.rect, color));
                }
                pane.into_any_element()
            })
            .collect()
    }

    /// Paint each live pane divider above the grids it separates.
    fn render_dividers(&self, cx: &App) -> Vec<gpui::AnyElement> {
        self.shell
            .dividers(self.pane_viewport(), cx)
            .into_iter()
            .map(|divider| {
                div()
                    .absolute()
                    .left(px(divider.rect.x))
                    .top(px(divider.rect.y))
                    .w(px(divider.rect.width))
                    .h(px(divider.rect.height))
                    .bg(surface(self.chrome.divider, self.opacity))
                    .into_any_element()
            })
            .collect()
    }

    /// Translate a window pointer position into the grid band's local space.
    fn grid_local_position(&self, position: Point<Pixels>) -> Option<(f32, f32)> {
        let bounds = self.grid_area.get()?;
        Some((
            f32::from(position.x) - f32::from(bounds.origin.x),
            f32::from(position.y) - f32::from(bounds.origin.y),
        ))
    }

    /// Start a drag when the pointer lands in a divider's hit band.
    fn press_divider(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) -> bool {
        let Some((x, y)) = self.grid_local_position(position) else { return false };
        let viewport = self.pane_viewport();
        let dividers = self.shell.dividers(viewport, cx);
        let Some(divider) = divider::hit_test_divider(&dividers, x, y) else {
            return false;
        };
        self.pointer.divider_drag = Some(divider::start_drag(divider, viewport));
        true
    }

    /// Apply an in-flight divider drag and republish both panes' geometry.
    fn drag_divider(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) -> bool {
        let Some(drag) = self.pointer.divider_drag else { return false };
        let Some((x, y)) = self.grid_local_position(position) else { return true };
        let mouse_pos = match drag.direction {
            SplitDirection::Horizontal => x,
            SplitDirection::Vertical => y,
        };
        if self.shell.set_pane_ratio(drag.first_pane, divider::drag_ratio(&drag, mouse_pos), cx) {
            self.after_layout_change(cx);
        }
        true
    }

    /// Resolve a left press over the grid band.
    ///
    /// Three consumers in priority order. The overlay scrollbar goes first
    /// because it is chrome painted over the cells: a click on the thumb was
    /// never meant for the application below it, which is the order the winit
    /// client resolved its chrome in too. A mouse-tracking application comes
    /// next, and only when it declines does the press mean selection.
    fn press_grid(&mut self, event: &MouseDownEvent, cx: &mut Context<Self>) {
        if self.press_divider(event.position, cx)
            || self.press_scrollbar(event.position, cx)
            || self.forward_mouse_press(event)
        {
            return;
        }
        self.click_grid(event.position, cx);
    }

    /// Resolve pointer motion over the grid band.
    ///
    /// An in-flight thumb drag owns the pointer outright. Otherwise hover is
    /// tracked even while an application owns the pointer — the hover widen is
    /// what makes the thumb grabbable, and a press on it would have been
    /// claimed by the scrollbar anyway — before the motion falls through to
    /// mouse reporting and then to extending a selection.
    fn move_over_grid(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        if self.drag_divider(event.position, cx) || self.drag_scrollbar(event.position, cx) {
            return;
        }
        self.update_scrollbar_hover(event.position, cx);
        if self.forward_mouse_motion(event) {
            return;
        }
        self.extend_selection(event.position, cx);
    }

    /// Resolve a left release over the grid band, ending whichever gesture the
    /// matching press started.
    fn release_over_grid(&mut self, event: &MouseUpEvent, cx: &mut Context<Self>) {
        if self.pointer.divider_drag.take().is_some()
            || self.release_scrollbar(cx)
            || self.forward_mouse_release(event)
        {
            return;
        }
        self.finish_selection(cx);
    }

    /// Collect everything `session_id`'s overlay scrollbar needs for one frame.
    ///
    /// Returns `None` when the session has no grid yet — there is no viewport
    /// to describe, so there is nothing to draw. The marks are cloned out of
    /// the shared store rather than borrowed because the drain writes them from
    /// another thread and the element outlives this call.
    fn scrollbar_paint(&self, session_id: SessionId) -> Option<ScrollbarPaint> {
        let state = Rc::clone(self.scrollbars.panes.get(&session_id)?);
        let metrics = self.shared.panes.lock().ok()?.scroll_metrics(session_id)?;
        let marks = self
            .shared
            .prompt_marks
            .lock()
            .map(|marks| marks.marks(session_id).to_vec())
            .unwrap_or_default();
        Some(ScrollbarPaint { state, metrics, marks, style: self.scrollbars.style })
    }

    /// Reveal `session_id`'s scrollbar and re-arm its idle timer.
    ///
    /// Called from every path that moves a viewport — the wheel, the scroll
    /// chords, the three mark-relative jumps, and the server's `ScrollBottom`
    /// snap — so the overlay is the confirmation that a scroll landed, which is
    /// the whole reason it fades in rather than being always on.
    fn pulse_scrollbar(&mut self, session_id: SessionId) {
        self.scrollbars.panes.entry(session_id).or_default().borrow_mut().on_scroll_action();
    }

    /// Reveal the focused pane's scrollbar, if a session is attached.
    fn pulse_focused_scrollbar(&mut self) {
        if let Some(session_id) = self.shared.active_session.lock().ok().and_then(|guard| *guard) {
            self.pulse_scrollbar(session_id);
        }
    }

    /// Advance every pane's fade animation, reporting whether any scrollbar is
    /// still visible and therefore still owes the window a frame.
    ///
    /// The fade is wall-clock, not frame-count, so it is driven from the idle
    /// tick as well as from paint: a scroll that lands on a silent pane must
    /// still fade out on time, and nothing else would wake the window.
    fn tick_scrollbar_fades(&mut self) -> bool {
        let mut animating = false;
        let offsets: HashMap<SessionId, usize> = self.shared.panes.lock().map_or_else(
            |_| HashMap::new(),
            |grids| {
                self.scrollbars
                    .panes
                    .keys()
                    .filter_map(|session| {
                        Some((*session, grids.scroll_metrics(*session)?.display_offset))
                    })
                    .collect()
            },
        );
        for (session_id, state) in &self.scrollbars.panes {
            let display_offset = offsets.get(session_id).copied().unwrap_or(0);
            let mut state = state.borrow_mut();
            let before = state.opacity;
            // `tick_fade` reports whether the scrollbar is still on screen, and
            // the tick that finally takes it to zero reports `false` — while
            // still owing exactly one more frame, the one that clears it.
            // Without the opacity comparison the window would keep the last
            // barely-visible thumb painted until something else repainted it.
            let visible = state.tick_fade(display_offset);
            animating |= visible || (state.opacity - before).abs() > f32::EPSILON;
        }
        animating
    }

    /// Idle-tick hook: advance the fades and repaint while any is still on
    /// screen. Returning early when nothing is animating keeps a rested window
    /// from repainting sixty times a second.
    fn poll_scrollbar_fades(&mut self, cx: &mut Context<Self>) {
        if self.tick_scrollbar_fades() {
            cx.notify();
        }
    }

    /// Drop scrollbar state for sessions that are no longer on screen.
    ///
    /// Keyed by session, so this is the same retirement the pane grids get: a
    /// closed tab must not leave its fade timer (or a stale drag) behind.
    fn retire_scrollbars(&mut self, live: &HashSet<SessionId>) {
        self.scrollbars.panes.retain(|session_id, _| live.contains(session_id));
        if self.scrollbars.drag.is_some_and(|session| !live.contains(&session)) {
            self.scrollbars.drag = None;
        }
    }

    /// The scrollbar placement for `session_id` against the last painted grid
    /// rect, or `None` when nothing has been painted or the session has no
    /// grid. Pointer hit-testing and drag math both resolve through this so
    /// they can never disagree with what paint drew.
    fn scrollbar_layout(&self, session_id: SessionId) -> Option<ScrollbarLayout> {
        let bounds = self.grid_bounds.get()?;
        let metrics = self.shared.panes.lock().ok()?.scroll_metrics(session_id)?;
        Some(ScrollbarLayout {
            pane_rect: Rect {
                x: f32::from(bounds.left()),
                y: f32::from(bounds.top()),
                width: f32::from(bounds.size.width),
                height: f32::from(bounds.size.height),
            },
            metrics,
            // The GPUI client's tab strip lives in the window titlebar, so the
            // pane reserves no strip of its own and the track is the full rect.
            tab_bar_height: 0.0,
        })
    }

    /// Track the pointer over the focused pane's scrollbar hit zone.
    ///
    /// Hover pins the overlay open and widens the thumb, which is what makes it
    /// grabbable: the resting 6 px thumb is a hint, and the 3x hit zone plus
    /// the widen are what turn it into a control.
    fn update_scrollbar_hover(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(session_id) = self.shared.active_session.lock().ok().and_then(|guard| *guard)
        else {
            return;
        };
        let Some(layout) = self.scrollbar_layout(session_id) else { return };
        let Some(state) = self.scrollbars.panes.get(&session_id) else { return };
        let width = state.borrow().current_width(self.scrollbars.style.width);
        let inside = hit_test_scrollbar(
            &layout,
            f32::from(position.x),
            f32::from(position.y),
            width.max(self.scrollbars.style.width),
        );
        let mut state = state.borrow_mut();
        if inside == state.hover {
            return;
        }
        if inside {
            state.on_hover_enter();
        } else {
            state.on_hover_leave();
        }
        drop(state);
        cx.notify();
    }

    /// Claim a left press that landed on the focused pane's scrollbar.
    ///
    /// A press on the thumb starts a drag; a press anywhere else in the hit
    /// zone jumps the viewport to that point on the track. Returns `true` when
    /// the press was consumed, so the caller leaves selection alone — the
    /// scrollbar is chrome painted over the grid, and a click on it was never
    /// meant for the cells underneath.
    fn press_scrollbar(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) -> bool {
        let Some(session_id) = self.shared.active_session.lock().ok().and_then(|guard| *guard)
        else {
            return false;
        };
        let Some(layout) = self.scrollbar_layout(session_id) else { return false };
        let Some(state) = self.scrollbars.panes.get(&session_id) else { return false };
        let (x, y) = (f32::from(position.x), f32::from(position.y));
        let width = state.borrow().current_width(self.scrollbars.style.width);
        if !hit_test_scrollbar(&layout, x, y, width) {
            return false;
        }
        if hit_test_thumb(&layout, x, y, width) {
            let mut state = state.borrow_mut();
            state.drag = Some(ScrollbarDrag {
                start_mouse_y: y,
                start_display_offset: layout.metrics.display_offset,
            });
            // A drag holds the overlay open by itself; clearing the timer keeps
            // it from fading out from under the pointer mid-drag.
            state.opacity = 1.0;
            state.fade_start = None;
            drop(state);
            self.scrollbars.drag = Some(session_id);
            cx.notify();
            return true;
        }
        let target = offset_from_track_click(&layout, y, width);
        self.scroll_to_offset(session_id, target, cx);
        true
    }

    /// Continue an in-flight thumb drag. Returns `true` while a drag owns the
    /// pointer, so motion never doubles as a selection extension.
    fn drag_scrollbar(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) -> bool {
        let Some(session_id) = self.scrollbars.drag else { return false };
        let Some(layout) = self.scrollbar_layout(session_id) else { return true };
        let Some(state) = self.scrollbars.panes.get(&session_id) else { return true };
        let width = state.borrow().current_width(self.scrollbars.style.width);
        let Some(drag) = state.borrow().drag else { return true };
        let target = offset_from_drag(&layout, &drag, f32::from(position.y), width);
        self.scroll_to_offset(session_id, target, cx);
        true
    }

    /// Finish a thumb drag, re-arming the fade unless the pointer is still
    /// hovering. Returns `true` when a drag was actually in flight.
    fn release_scrollbar(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(session_id) = self.scrollbars.drag.take() else { return false };
        if let Some(state) = self.scrollbars.panes.get(&session_id) {
            state.borrow_mut().on_drag_end();
        }
        cx.notify();
        true
    }

    /// Move `session_id`'s viewport to an absolute `display_offset`.
    ///
    /// Both scrollbar gestures land here so they share the repaint and the
    /// split-scroll bookkeeping `scroll_terminal` does for the keyboard: a
    /// deliberate scroll dissolves the pin exactly as `Scroll::Bottom` does.
    fn scroll_to_offset(&mut self, session_id: SessionId, target: usize, cx: &mut Context<Self>) {
        let Ok(mut grids) = self.shared.panes.lock() else { return };
        let terminal = grids.grid_mut(session_id);
        terminal.set_split_scroll_eligibility(SplitScrollEligibility::default());
        let moved = terminal.scroll_to_offset(target);
        drop(grids);
        if !moved {
            return;
        }
        self.pulse_scrollbar(session_id);
        tracing::info!(%session_id, target, "scrollbar moved the viewport");
        cx.notify();
    }

    /// Open the command palette, building its entry list from the live update /
    /// profile state, and subscribe to its confirm/dismiss events so a choice or
    /// an outside click tears the overlay down.
    fn open_command_palette(&mut self, cx: &mut Context<Self>) {
        self.context_menu = None;
        let profile_names = scribe_common::profiles::list_profiles().unwrap_or_default();
        let active = scribe_common::profiles::active_profile_name().ok();
        // The conditional "Update Scribe to vX" row reads the SAME
        // `UpdateAvailable` broadcast the centred status-bar CTA does, so the two
        // affordances can never disagree about whether an update is offered.
        let update_version =
            self.shared.update.lock().ok().and_then(|state| state.version().map(str::to_owned));
        let entries = build_entries(update_version.as_deref(), &profile_names, active.as_deref());
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
    ) -> Vec<scribe_client::search::MatchHighlight> {
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
            has_selection: self
                .with_focused_grid(|terminal| terminal.has_selection())
                .unwrap_or(false),
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
        if self.hits_split_scroll_chip(position) {
            tracing::info!("split-scroll jump chip clicked");
            self.scroll_terminal(Scroll::Bottom, cx);
            return;
        }
        self.begin_selection(position, cx);
    }

    /// Whether `position` landed on the docked split-scroll jump chip, which
    /// owns the click ahead of the selection gesture because it is a control
    /// drawn over the grid rather than content in it.
    fn hits_split_scroll_chip(&self, position: Point<gpui::Pixels>) -> bool {
        if self.split_scroll.pin_height <= 0.0 {
            return false;
        }
        let Some(bounds) = self.grid_bounds.get() else {
            return false;
        };
        let (rows, pin_rows) = self
            .with_focused_grid(|terminal| (terminal.content().rows.len(), terminal.pin_rows()))
            .unwrap_or((0, 0));
        hits_jump_chip(bounds, &self.font, rows, pin_rows, position)
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
            this.route_dialog_outcome(*outcome, ctx);
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

    /// The workspace the notes surface belongs to: the one the user is in.
    ///
    /// The focused region wins whenever the server itself minted its id, which
    /// is what makes a split window open notes for the region under the cursor
    /// rather than for the window as a whole. A region the client opened but
    /// the server has not answered for yet is not a workspace the server can
    /// look notes up by, so the fallback is the focused tab's workspace — the
    /// id `SessionList` / `SessionCreated` filed that session under.
    ///
    /// `None` means the client has not yet learned any server workspace, and
    /// the callers treat that as "nothing to ask about" instead of inventing an
    /// id the server has never seen.
    fn notes_workspace_id(&self, cx: &App) -> Option<WorkspaceId> {
        let focused = self.shell.focused_workspace_id(cx);
        if self.shell.is_server_workspace(focused) {
            return Some(focused);
        }
        self.shared.tabs.lock().ok().and_then(|tabs| tabs.active_workspace())
    }

    /// Open the workspace-notes modal on the focused workspace, seeded from the
    /// cache the reader keeps, and ask the server for its authoritative copy.
    ///
    /// Nothing is fabricated here: the modal is bound to the very
    /// [`WorkspaceId`] the server knows the focused pane by, so the
    /// `WorkspaceNotesGet` it puts on the wire names a workspace the server can
    /// answer for, and the `WorkspaceNotesMutate` a save produces is filed
    /// against real notes. The reply lands on the reader thread and is folded
    /// in by [`Self::sync_workspace_notes`] on the next redraw.
    fn open_workspace_notes_modal(&mut self, cx: &mut Context<Self>) {
        self.workspace_notes_preview = WorkspaceNotesPreviewSurface::default();
        self.command_palette = None;
        self.context_menu = None;
        let Some(workspace_id) = self.notes_workspace_id(cx) else {
            tracing::warn!("workspace notes requested before any server workspace is known");
            return;
        };
        let colors = WorkspaceNotesModalColors::from(&self.chrome);
        let (draft, active, archived, cached_error, adopted) =
            self.shared.notes.lock().map_or_else(
                |_| (String::new(), Vec::new(), Vec::new(), None, 0),
                |store| {
                    (
                        store.draft_text(workspace_id),
                        store.active_notes(workspace_id),
                        store.archived_notes(workspace_id),
                        store.last_error().map(str::to_owned),
                        store.version(),
                    )
                },
            );
        self.workspace_notes_adopted = adopted;
        let modal = cx.new(|cx| {
            let mut view = WorkspaceNotesModalView::new(&colors, cx);
            view.open(workspace_id, draft, cx);
            view.set_notes(active, archived, cx);
            view.set_error(cached_error, cx);
            view
        });
        if let Err(error) = self.sink.workspace_notes_get(vec![workspace_id]) {
            tracing::warn!(%error, "workspace notes get dropped: IPC writer closed");
        }
        cx.subscribe(&modal, |this, modal, action, ctx| {
            this.route_workspace_notes_action(&modal, action, ctx);
        })
        .detach();
        self.workspace_notes_modal = Some(modal);
        tracing::info!(%workspace_id, "opened the workspace notes modal");
        cx.notify();
    }

    /// Show or hide the compact notes preview bound to the focused workspace.
    ///
    /// The titlebar owns the hover affordance, while this shell owns the
    /// server-backed data and the preview entity. Keeping that boundary makes
    /// the view a normal overlay: opening the modal clears it, and every hover
    /// begins with a fresh `WorkspaceNotesGet` instead of inventing local data.
    fn set_workspace_notes_preview(&mut self, hovered: bool, cx: &mut Context<Self>) {
        if !hovered {
            if self.workspace_notes_preview.view.take().is_some() {
                self.workspace_notes_preview.workspace_id = None;
                cx.notify();
            }
            return;
        }
        if self.workspace_notes_modal.is_some() || self.workspace_notes_preview.view.is_some() {
            return;
        }
        let Some(workspace_id) = self.notes_workspace_id(cx) else {
            return;
        };
        let (summaries, total_count, adopted) = self.shared.notes.lock().map_or_else(
            |_| (Vec::new(), 0, 0),
            |store| {
                let (summaries, total_count) =
                    store.hover_summaries(workspace_id, MAX_PREVIEW_ROWS, 56);
                (summaries, total_count, store.version())
            },
        );
        let colors = WorkspaceNotesPreviewColors::from(&self.chrome);
        let preview = cx.new(|cx| {
            let mut view = WorkspaceNotesPreviewView::new(colors);
            view.set_summaries(summaries, total_count, cx);
            view
        });
        cx.subscribe(&preview, move |this, _preview, action, ctx| match action {
            WorkspaceNotesPreviewAction::OpenEditor => this.open_workspace_notes_modal(ctx),
            WorkspaceNotesPreviewAction::ArchiveNote(note_id) => {
                this.send_workspace_notes_mutation(
                    scribe_common::protocol::WorkspaceNotesMutation::ArchiveNote {
                        workspace_id,
                        note_id: note_id.clone(),
                        reason: ArchiveReason::Done,
                    },
                );
            }
            WorkspaceNotesPreviewAction::FocusEditor => {}
        })
        .detach();
        self.workspace_notes_preview.view = Some(preview);
        self.workspace_notes_preview.workspace_id = Some(workspace_id);
        self.workspace_notes_preview.adopted = adopted;
        if let Err(error) = self.sink.workspace_notes_get(vec![workspace_id]) {
            tracing::warn!(%error, "workspace notes hover get dropped: IPC writer closed");
        }
        cx.notify();
    }

    /// Fold the newest server notes into the open modal.
    ///
    /// Runs on every redraw for the same reason [`Self::sync_find_results`]
    /// does: the snapshot reply and the change broadcast both arrive on the IPC
    /// reader thread, which cannot touch a GPUI entity. The store's version is
    /// the gate, so an unchanged cache costs one comparison and never clobbers
    /// what the user is typing; the draft is only replaced while it is pristine
    /// ([`WorkspaceNotesModalView::replace_pristine_draft`]), which is what
    /// keeps a late snapshot from eating typed text.
    fn sync_workspace_notes(&mut self, cx: &mut Context<Self>) {
        self.sync_workspace_notes_modal(cx);
        self.sync_workspace_notes_preview(cx);
    }

    fn sync_workspace_notes_modal(&mut self, cx: &mut Context<Self>) {
        let Some(modal) = self.workspace_notes_modal.clone() else { return };
        let Some(workspace_id) = modal.read(cx).workspace_id() else { return };
        let Ok(store) = self.shared.notes.lock() else { return };
        if store.version() == self.workspace_notes_adopted {
            return;
        }
        self.workspace_notes_adopted = store.version();
        let active = store.active_notes(workspace_id);
        let archived = store.archived_notes(workspace_id);
        let draft = store.draft_text(workspace_id);
        let error = store.last_error().map(str::to_owned);
        modal.update(cx, |view, ctx| {
            view.set_notes(active, archived, ctx);
            view.replace_pristine_draft(draft, ctx);
            view.set_error(error, ctx);
        });
    }

    fn sync_workspace_notes_preview(&mut self, cx: &mut Context<Self>) {
        let Some(preview) = self.workspace_notes_preview.view.clone() else { return };
        let Some(workspace_id) = self.workspace_notes_preview.workspace_id else { return };
        let Ok(store) = self.shared.notes.lock() else { return };
        if store.version() == self.workspace_notes_preview.adopted {
            return;
        }
        self.workspace_notes_preview.adopted = store.version();
        let (summaries, total_count) = store.hover_summaries(workspace_id, MAX_PREVIEW_ROWS, 56);
        preview.update(cx, |view, ctx| view.set_summaries(summaries, total_count, ctx));
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
                        scribe_client::workspace_notes_modal::WorkspaceNotesView::Active,
                        ctx,
                    );
                });
            }
            WorkspaceNotesModalAction::ShowArchive => {
                modal.update(cx, |m, ctx| {
                    m.set_view(
                        scribe_client::workspace_notes_modal::WorkspaceNotesView::Archive,
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
        // Feature 013 (T017): a displaced window is frozen. Checked before
        // everything else — including the share prompt and the shell chords —
        // because ALL input is suppressed while another controller holds the
        // window; Enter is its one affordance and every other key is swallowed
        // rather than reaching a binding, an overlay, or the PTY.
        if self.window_displaced() {
            if LostControlState::reclaim_requested(ReclaimKey::from_keystroke(
                event.keystroke.key.as_str(),
            )) {
                self.reclaim_window(cx);
            }
            return true;
        }
        // Feature 015 (T020): a pending control request is a full-window modal —
        // the holder (or the owner while control is unheld) answers it before
        // anything else reaches a binding, an overlay, or the PTY.
        if self.share_prompt_pending() && self.run_share_key(event, cx) {
            return true;
        }
        let overlay_free = self.dialog.is_none()
            && self.workspace_notes_modal.is_none()
            && self.find_overlay.is_none()
            && !self.remote_connect.is_active();

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

        if self.handle_remote_connect_key(event, cx) {
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
            // A composition belongs to the window that owns the keyboard; the
            // OS input method has already moved on, so keeping the overlay up
            // would strand it over a pane nobody is typing into.
            self.clear_preedit(cx);
            return;
        }
        if let Some(guard) = self.x11_focus.as_mut() {
            guard.clear_reactivation_debounce();
        }
        // Focus-on-activate fallback: a notification service that activates the
        // app without reporting which toast was clicked leaves this as the only
        // link between the click and the pane that asked for attention. The
        // click-reporting path consumes the same token, so a dispatcher that
        // does report the click never double-switches.
        if let Some(session_id) =
            self.notifications.center.update(cx, |center, _| center.take_pending_focus())
        {
            self.focus_notified_session(session_id, window, cx);
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
        tracing::debug!(key = %event.keystroke.key, "encoding a keystroke for the PTY");
        if let Some(session_id) =
            self.shared.active_session.lock().ok().and_then(|session| *session)
            && let Ok(mut ai) = self.shared.ai.lock()
        {
            ai.tracker.clear_attention_states(session_id);
        }
        // A keystroke that reaches the byte encoder was not consumed by the
        // input method, so any composition still on screen is stale. GPUI's
        // xkb-compose path makes this load-bearing: it marks a dead key as
        // preedit and then delivers the composed character as an ordinary
        // `KeyDown` without ever retracting the mark.
        self.clear_preedit(cx);
        // Typing dismisses a selection, exactly as it does in the winit client:
        // the highlighted region describes content the shell is about to
        // overwrite, so leaving it up would highlight the wrong cells.
        if self.with_focused_grid(DisplayOnlyTerminal::clear_selection) == Some(true) {
            cx.notify();
        }
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
    /// [`ShareChrome::intercept_key`](scribe_client::share::ShareChrome::intercept_key);
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
        scribe_common::perf_probe::record_input_sent(session_id);
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

/// Build the spawn command for an AI tab, matching the legacy client.
///
/// The CLI starts through the user's login shell (`-lic` + `exec`) so it
/// inherits the same PATH and rc files a normal tab would, without first
/// rendering a shell prompt.
/// Where an [`AutomationAction`] came from, which decides what happens to one
/// this shell cannot run.
///
/// A LOCAL action (a command-palette row) may be offered to the window's
/// controller with `DispatchAction`; a SERVER one arrived as the `RunAction`
/// that dispatch produces, so bouncing it back would loop forever.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActionOrigin {
    /// Raised by this window's own UI.
    Local,
    /// Delivered by the server as a `RunAction`.
    Server,
}

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

/// Narrow a viewport cell index to the `u16` axis a mouse report carries.
///
/// The protocol's own X10 form cannot address past 223 columns anyway, and the
/// encoders clamp there themselves, so saturating at `u16::MAX` is a formality
/// that keeps the conversion lossless-by-construction rather than a cast.
fn report_axis(value: usize) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

/// Render report bytes for the log with the control bytes escaped.
///
/// A mouse report is almost entirely unprintable, so the raw bytes would reach
/// the log as invisible escape sequences that reprogram whatever terminal is
/// tailing it. Escaping keeps the exact sequence assertable from a script.
fn escape_report_bytes(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| match byte {
            0x20..=0x7e => char::from(*byte).to_string(),
            other => format!("\\x{other:02x}"),
        })
        .collect()
}

/// Keystroke encoder feeding the outbound [`IpcSink`] (see
/// [`TerminalView::on_key_down`]).
///
/// This is the live entry point of the ported terminal encoder: the GPUI event
/// is lowered by [`KeyInput::from_key_down`] and handed to
/// [`input::encode`](scribe_client::input::encode), the same function the
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
/// The same 16 ms tick is the idle-wake boundary two wall-clock surfaces expire
/// on: the feature-015 control hint (a hint set five seconds ago must clear even
/// on a window whose output has gone quiet, which by definition never bumps the
/// generation) and the overlay scrollbar's fade, which has to run its 1.5 s idle
/// delay and 0.3 s ramp down to nothing after the last scroll.
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
            let idle = view.update(app, |view, view_cx| {
                view.expire_share_hint(view_cx);
                view.poll_scrollbar_fades(view_cx);
                view.tick_ai_animation(view_cx);
            });
            if idle.is_err() {
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
        // Feature 013/014: the transport label is a `&'static str`, so the
        // tailnet chrome lock is released before `build_model` borrows anything
        // else rather than being held across the whole build.
        let remote_transport =
            self.shared.remote.lock().ok().and_then(|remote| remote.transport_label());
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
                // Feature 014 (T025): the controlling-side transport indicator,
                // present only on a client that itself reached its window over a
                // remote transport.
                remote_transport,
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

    /// The invisible canvas that measures the grid band and republishes the
    /// pane geometry whenever the band moved.
    ///
    /// The band's height is whatever the chrome bands leave over, which no
    /// arithmetic on the window size can predict (the prompt strip comes and
    /// goes with the pane's prompts), so the painted rect is the only honest
    /// source for the cell counts the server is told about.
    ///
    /// The rect lands during prepaint, i.e. *after* the `render` that built
    /// this canvas already ran [`Self::sync_grid_geometry`] against the
    /// previous frame's area. A window resize repaints exactly once, so on the
    /// render path alone nothing would ever compare the new area against the
    /// published one: the panes would be re-laid locally while every PTY kept
    /// its pre-resize size. The measuring write is therefore what asks for the
    /// follow-up, deferred so the publish still runs on the view rather than
    /// mid-paint, and only on the frame the band actually moved.
    fn grid_area_probe(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let area = Rc::clone(&self.grid_area);
        let handle = cx.weak_entity();
        canvas(
            move |bounds, _window, app| {
                if !record_grid_area(&area, bounds) {
                    return;
                }
                app.defer(move |app| {
                    handle.update(app, TerminalView::sync_grid_geometry).ok();
                });
            },
            |_, (), _, _| {},
        )
        .absolute()
        .size_full()
        .into_any_element()
    }

    /// Lower the live share state onto the overlay layer: the presence roster,
    /// the transient control hint, and the modal grant/deny prompt.
    /// Build the terminal-grid band: the pane layout's canvas.
    ///
    /// Every pane is positioned inside it as a fraction of its size, so the
    /// split ratios need no device-pixel measurement, and the band itself
    /// carries the right-click that opens the context menu at the cursor
    /// without disturbing the display-only elements inside it.
    fn render_grid(&mut self, ime: ImePaint, cx: &mut Context<Self>) -> gpui::AnyElement {
        let focused = self.sync_split_scroll();
        let panes = self.render_panes(focused, ime, cx);
        let dividers = self.render_dividers(cx);
        div()
            .flex_1()
            .relative()
            .bg(surface(self.terminal_colors.background, self.opacity))
            .child(self.grid_area_probe(cx))
            .children(panes)
            .children(dividers)
            // The wheel is claimed by the application when it tracks the mouse,
            // by the alternate screen's 1007 fallback, or — the ordinary case —
            // by this client's own scrollback viewport.
            .on_scroll_wheel(cx.listener(|view, event: &ScrollWheelEvent, _window, ctx| {
                view.scroll_wheel(event, ctx);
            }))
            // Every button gesture below asks the mouse reporter first: an
            // application that enabled tracking owns the pointer, and only when
            // it declines (or Shift takes the pointer back) does the click mean
            // selection / primary paste / context menu.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|view, event: &MouseDownEvent, _window, ctx| {
                    view.press_grid(event, ctx);
                }),
            )
            .on_mouse_move(cx.listener(|view, event: &MouseMoveEvent, _window, ctx| {
                view.move_over_grid(event, ctx);
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|view, event: &MouseUpEvent, _window, ctx| {
                    view.release_over_grid(event, ctx);
                }),
            )
            // Middle click is the X11 primary-selection paste, and it is a
            // paste like any other: it goes through the same spec-011 gate the
            // chord uses rather than straight to the PTY.
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(|view, event: &MouseDownEvent, _window, ctx| {
                    if view.forward_mouse_press(event) {
                        return;
                    }
                    view.paste_primary(ctx);
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|view, event: &MouseDownEvent, _window, ctx| {
                    if view.forward_mouse_press(event) {
                        return;
                    }
                    view.open_context_menu(event.position, ctx);
                }),
            )
            // Middle and right releases exist only for the reporter: they carry
            // no client-side gesture, but an application in mode 1002 needs the
            // button-up that ends its drag, and the tracked button has to be
            // dropped either way.
            .on_mouse_up(
                MouseButton::Middle,
                cx.listener(|view, event: &MouseUpEvent, _window, _ctx| {
                    view.forward_mouse_release(event);
                }),
            )
            .on_mouse_up(
                MouseButton::Right,
                cx.listener(|view, event: &MouseUpEvent, _window, _ctx| {
                    view.forward_mouse_release(event);
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
            let display = scribe_client::tooltip::truncate_url(
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

    fn build_workspace_notes_preview_overlay(&self) -> Option<gpui::AnyElement> {
        self.workspace_notes_preview.view.clone().map(|preview| {
            div()
                .absolute()
                .top(px(TITLEBAR_HEIGHT))
                .right(px(154.0))
                .child(preview)
                .into_any_element()
        })
    }

    /// Build the active remote-connect picker above normal terminal chrome.
    fn build_remote_picker_overlay(&self) -> Option<gpui::AnyElement> {
        self.remote_connect.is_active().then(|| {
            let colors = RemotePickerColors::from(&self.chrome);
            remote_picker_overlay(&self.remote_connect.view(), &colors)
        })
    }

    /// Restore focus when another surface left the terminal window unfocused.
    fn ensure_focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.focus_handle.is_focused(window) {
            window.focus(&self.focus_handle, cx);
        }
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        log_first_frame_timing();
        self.report_perf_frame();
        self.ensure_focus(window, cx);
        self.capture_geometry(window, cx);
        self.sync_tabs(cx);
        self.sync_find_results(cx);
        self.sync_workspace_notes(cx);
        self.sync_remote_connect();
        self.reconcile_panes(cx);
        self.sync_grid_geometry(cx);
        let ime = self.sync_ime(cx);
        let grid = self.render_grid(ime, cx);
        let status = self
            .shared
            .status
            .lock()
            .map_or_else(|_| "terminal state unavailable".to_owned(), |guard| guard.clone());
        let status_bar = self.render_status_bar(cx);
        let prompt_model = self.build_prompt_model();
        let prompt_colors = self.prompt_colors.with_opacity(self.opacity);
        let prompt_strip = prompt_model.map(|prompt| {
            prompt_bar::render(&prompt, &prompt_colors, f32::from(CELL_HEIGHT), None)
                .into_any_element()
        });
        let tooltip = self.build_tooltip_demo();
        let share = self.build_share_overlay();
        let remote_picker = self.build_remote_picker_overlay();
        let displaced = self.build_lost_control_overlay(cx);
        let notes_preview = self.build_workspace_notes_preview_overlay();
        let opacity = self.opacity;
        // The root itself paints nothing. Every band below fills the window
        // edge to edge, so leaving the root unfilled guarantees each pixel
        // carries the opacity alpha exactly once instead of compositing a
        // translucent band over a translucent root and coming out more opaque
        // than the configured value.
        div()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|view, event: &KeyDownEvent, _window, ctx| {
                // Claim every key the window sees, at every level below.
                //
                // Once an input handler is registered — which the IME wiring
                // now does on every frame — gpui's platform layer follows an
                // un-stopped `KeyDown` with
                // `input_handler.replace_text_in_range(key_char)`, the "insert
                // the typed character into the focused text field" behaviour a
                // text editor wants. A terminal has already encoded that
                // keystroke itself, so letting it through types every printable
                // character twice, and turns keys consumed by vi mode or a
                // binding into stray PTY bytes. A genuine input method never
                // uses that path: composed text arrives through the platform's
                // own commit callback, which propagation does not gate.
                ctx.stop_propagation();
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
            // File drop from the compositor. GPUI lowers an external file drop
            // onto an ordinary drag whose payload is `ExternalPaths`, so the
            // drop listener goes on the root — the whole window is a valid drop
            // target, exactly as it was under winit's `DroppedFile`.
            .on_drop(cx.listener(|view, paths: &gpui::ExternalPaths, _window, ctx| {
                view.handle_dropped_paths(paths.paths(), ctx);
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
            .children(notes_preview)
            .children(share)
            .children(tooltip)
            .children(remote_picker)
            // Last child, so the frozen banner covers every other overlay: while
            // a remote controller holds this window there is nothing else to
            // interact with.
            .children(displaced)
    }
}

/// Apply a picker result on the window thread, logging only a dropped view.
fn update_remote_picker(
    view: &WeakEntity<TerminalView>,
    app: &mut AsyncApp,
    update: impl FnOnce(&mut TerminalView, &mut Context<TerminalView>),
) {
    if let Err(error) = view.update(app, |this, view_cx| update(this, view_cx)) {
        tracing::debug!(?error, "remote picker update dropped with its window");
    }
}

/// Show the interim LAN approval state on the GPUI-owned picker.
fn mark_remote_picker_awaiting_approval(view: &WeakEntity<TerminalView>, app: &mut AsyncApp) {
    update_remote_picker(view, app, |this, view_cx| {
        this.remote_connect.on_awaiting_approval();
        view_cx.notify();
    });
}

/// A Tokio-owned picker probe result, applied only after hopping back to the
/// foreground thread that owns the GPUI view.
enum RemotePickerProbe {
    Windows { host: String, port: u16, windows: Vec<WindowInfo> },
    TailnetFailure(RemoteConnectOutcome),
    LanFailure(LanConnectOutcome),
}

/// Fold a completed remote-picker probe into the GPUI-owned picker state.
fn apply_remote_picker_probe(
    view: &WeakEntity<TerminalView>,
    app: &mut AsyncApp,
    probe: RemotePickerProbe,
) {
    update_remote_picker(view, app, |this, view_cx| {
        match probe {
            RemotePickerProbe::Windows { host, port, windows } => {
                this.remote_connect.set_windows(&host, port, windows);
            }
            RemotePickerProbe::TailnetFailure(outcome) => {
                this.remote_connect.on_dial_outcome(outcome);
            }
            RemotePickerProbe::LanFailure(outcome) => {
                this.remote_connect.on_lan_dial_outcome(outcome);
            }
        }
        view_cx.notify();
    });
}

/// Probe the TLS-and-approval LAN path before presenting the peer's windows.
async fn probe_lan_picker_windows(
    host: String,
    port: u16,
    pending_tx: UnboundedSender<()>,
) -> RemotePickerProbe {
    let Ok(dialer) = LanDialer::build(host.clone(), port).await else {
        return RemotePickerProbe::LanFailure(LanConnectOutcome::ConnectionFailure);
    };
    let Ok(mut stream) = dialer.connect().await else {
        return RemotePickerProbe::LanFailure(LanConnectOutcome::ConnectionFailure);
    };
    let outcome = lan_dial::handshake(&mut stream, lan_dial::local_device_name(), move || {
        _ = pending_tx.send(());
    })
    .await;
    if outcome != LanConnectOutcome::Accepted {
        return RemotePickerProbe::LanFailure(outcome);
    }
    let reply = async {
        write_message(&mut stream, &ClientMessage::ListWindows).await?;
        read_message::<ServerMessage, _>(&mut stream).await
    }
    .await;
    match reply {
        Ok(ServerMessage::WindowList { windows }) => {
            RemotePickerProbe::Windows { host, port, windows }
        }
        Ok(other) => {
            tracing::warn!(?other, "unexpected LAN picker window-probe reply");
            RemotePickerProbe::LanFailure(LanConnectOutcome::ConnectionFailure)
        }
        Err(error) => {
            tracing::warn!(%error, "LAN picker window probe failed");
            RemotePickerProbe::LanFailure(LanConnectOutcome::ConnectionFailure)
        }
    }
}

/// Probe the tailnet path before presenting the peer's windows.
async fn probe_tailnet_picker_windows(host: String, port: u16) -> RemotePickerProbe {
    let dialer = RemoteDialer::new(host.clone(), port);
    let Ok(mut stream) = dialer.connect().await else {
        return RemotePickerProbe::TailnetFailure(RemoteConnectOutcome::ConnectionFailure);
    };
    let outcome = remote_handshake::perform_remote_handshake(
        &mut stream,
        remote_handshake::local_device_name(),
    )
    .await;
    if outcome != RemoteConnectOutcome::Accepted {
        return RemotePickerProbe::TailnetFailure(outcome);
    }
    let reply = async {
        write_message(&mut stream, &ClientMessage::ListWindows).await?;
        read_message::<ServerMessage, _>(&mut stream).await
    }
    .await;
    match reply {
        Ok(ServerMessage::WindowList { windows }) => {
            RemotePickerProbe::Windows { host, port, windows }
        }
        Ok(other) => {
            tracing::warn!(?other, "unexpected remote picker window-probe reply");
            RemotePickerProbe::TailnetFailure(RemoteConnectOutcome::ConnectionFailure)
        }
        Err(error) => {
            tracing::warn!(%error, "remote picker window probe failed");
            RemotePickerProbe::TailnetFailure(RemoteConnectOutcome::ConnectionFailure)
        }
    }
}

/// Spawn the selected remote-control window with the transport markers consumed
/// by [`run_connection`]. The picker never repurposes this local window: a
/// remote attachment is a fresh client process with its own GPUI window.
fn spawn_remote_picker_client(
    transport: PeerTransport,
    host: &str,
    port: u16,
    window_id: Option<WindowId>,
) {
    let exe = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("scribe-client"));
    let mut command = std::process::Command::new(&exe);
    match transport {
        PeerTransport::Tailnet => {
            command.env(remote_handshake::REMOTE_DIAL_ENV, format!("{host}:{port}"));
            command.env_remove(remote_handshake::LAN_DIAL_ENV);
        }
        PeerTransport::Lan => {
            command.env(remote_handshake::LAN_DIAL_ENV, format!("{host}:{port}"));
            command.env_remove(remote_handshake::REMOTE_DIAL_ENV);
        }
    }
    if let Some(window_id) = window_id {
        command.env(remote_handshake::REMOTE_WINDOW_ENV, window_id.to_full_string());
    } else {
        command.env_remove(remote_handshake::REMOTE_WINDOW_ENV);
    }
    command.env_remove(remote_handshake::REMOTE_TAKEOVER_ENV);
    match command.spawn() {
        Ok(child) => {
            tracing::info!(pid = child.id(), %host, port, ?transport, ?window_id, "spawned remote picker client");
        }
        Err(error) => {
            tracing::warn!(exe = %exe.display(), %error, "failed to spawn remote picker client");
        }
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
        session_list_seen: Arc::new(AtomicBool::new(false)),
        ai: Arc::new(Mutex::new(AiChrome::new(
            load_config().unwrap_or_default().terminal.ai_session.ai_states,
        ))),
        chrome_metadata: Arc::new(Mutex::new(ChromeMetadata::new())),
        share: Arc::new(Mutex::new(ShareChrome::new())),
        update: Arc::new(Mutex::new(UpdateState::default())),
        lifecycle: Arc::new(Mutex::new(WindowLifecycle::new())),
        bells: Arc::new(Mutex::new(Vec::new())),
        ai_notices: Arc::new(Mutex::new(Vec::new())),
        notification_focus: Arc::new(Mutex::new(Vec::new())),
        find: Arc::new(Mutex::new(FindResults::default())),
        lan: Arc::new(Mutex::new(LanChrome::new())),
        prompt_marks: Arc::new(Mutex::new(PromptMarks::new())),
        workspaces: Arc::new(Mutex::new(Vec::new())),
        notes: Arc::new(Mutex::new(WorkspaceNotesStore::new())),
        clipboard: Arc::new(Mutex::new(ClipboardBridge::default())),
        remote: Arc::new(Mutex::new(RemoteChrome::new())),
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
        in_rx: Some(in_rx),
        size: terminal_size,
    });

    (shared, sink)
}

fn main() -> std::process::ExitCode {
    PROCESS_START.get_or_init(Instant::now);
    init_tracing();
    if std::env::args().skip(1).any(|arg| arg == "--vulkan-probe") {
        if let Err(error) = probe_vulkan() {
            tracing::error!(%error, "Scribe Vulkan probe failed");
            return std::process::ExitCode::FAILURE;
        }
        return std::process::ExitCode::SUCCESS;
    }
    // Arm the perf rig's runtime probe before anything can paint or type; it
    // stays inert unless `SCRIBE_PERF_PROBE` names a report path.
    scribe_common::perf_probe::init_from_env();

    // `scribe-client --settings` opens (or focuses) the settings window instead
    // of the terminal shell. The singleton absorbs the old scribe-settings
    // `settings.lock`/`settings.sock`: a second launch hands focus to the
    // running window and exits here.
    if std::env::args().skip(1).any(|arg| arg == "--settings") {
        run_settings();
        return std::process::ExitCode::SUCCESS;
    }

    // Claimed before the backend connects: the claim decides how many
    // `--restore-child` siblings this process fans out, and its geometry is what
    // the window is opened at.
    let cold_start = ColdStart::resolve();
    let terminal_size = default_terminal_size();
    let (shared, sink) = start_window_backend(terminal_size);

    application().run(move |cx: &mut App| {
        // Picker probes use Tokio networking while all view mutations return
        // through GPUI tasks, so both runtimes remain on their owning threads.
        gpui_tokio::init(cx);
        // Register the embedded Symbols Nerd Font before anything shapes a
        // line: `load_family` caches per-family lookups, so a later add could
        // never displace a cached miss for the fallback chain's first entry.
        scribe_client::fonts::register_embedded_fonts(cx);
        // Resolve the motion policy from `appearance.animations` (default true)
        // and the SCRIBE_DISABLE_ANIMATIONS override, then mirror it onto GPUI's
        // global reduce-motion flag so any UI transitions stay off — and
        // screenshots stay byte-identical — under the E2E determinism path.
        let animations = load_config().map_or(true, |config| config.appearance.animations);
        AnimationSettings::resolve(animations).apply_to_app(cx);
        open_window(cx, &shared, &sink, terminal_size, cold_start);
        cx.activate(true);
    });
    std::process::ExitCode::SUCCESS
}

/// Verify that the Vulkan loader can initialize a usable adapter before an
/// installer replaces a running client. The regular request tries hardware
/// first; wgpu's fallback request then admits Mesa's lavapipe adapter when no
/// hardware ICD is usable. No window or IPC connection is opened here.
fn probe_vulkan() -> Result<(), String> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::VULKAN,
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });
    let hardware = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        force_fallback_adapter: false,
        ..Default::default()
    }));
    let adapter = match hardware {
        Ok(adapter) => adapter,
        Err(_) => pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            force_fallback_adapter: true,
            ..Default::default()
        }))
        .map_err(|error| format!("no hardware or lavapipe adapter: {error}"))?,
    };
    pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
        .map(|_| ())
        .map_err(|error| format!("could not create a Vulkan device: {error}"))
}

/// Run the settings-only flow for `--settings`: enforce the singleton, then
/// open the GPUI settings window. When another instance already holds the
/// socket, [`singleton::acquire`] hands it focus and we exit without opening a
/// duplicate window.
fn run_settings() {
    use scribe_client::settings::singleton::{self, SingletonResult};

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
        // The handle is only useful to a caller that can be asked twice; this
        // process exists to show one settings window and exit with it.
        open_settings_window(cx);
        cx.activate(true);
    });

    // Hold the singleton guards for the window's lifetime, then clean up.
    singleton::cleanup_socket(&socket_path);
    drop(listener);
    drop(lock_file);
}

fn open_window(
    cx: &mut App,
    shared: &Shared,
    sink: &IpcSink,
    terminal_size: TerminalSize,
    cold_start: ColdStart,
) {
    let bounds = Bounds::centered(None, startup_window_size(cx), cx);
    // Restored geometry wins over the grid-derived startup size, and it is
    // applied at creation rather than after: GPUI takes the bounds (and the
    // maximized state) as window options, so a restored window never flashes at
    // the default size the way the winit client's async resize did.
    let window_bounds = cold_start
        .geometry
        .as_ref()
        .map_or(WindowBounds::Windowed(bounds), |geom| window_bounds_for(geom, bounds));
    if let Some(geom) = cold_start.geometry.as_ref() {
        tracing::info!(
            width = geom.width,
            height = geom.height,
            maximized = geom.maximized,
            "restoring persisted window geometry"
        );
    }
    let restored = cold_start.snapshot;
    let shared = shared.clone();
    let sink = sink.clone();
    // Everything between here and the root-view builder below happens inside
    // gpui: window creation, wgpu adapter enumeration, device creation and
    // surface configure. Timing it separates the platform GPU bring-up floor
    // from Scribe's own startup work for the perf gate.
    let bringup_start = Instant::now();
    if let Err(error) = cx.open_window(
        WindowOptions {
            window_bounds: Some(window_bounds),
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
            cx.new(|cx| {
                TerminalView::new(shared, sink, WindowSeed { terminal_size, restored }, window, cx)
            })
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
    in_rx: Option<UnboundedReceiver<InboundEvent>>,
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
        runtime.block_on(async move { supervise_connection(ctx).await });
    });
}

/// Keep a local client attached across a server handoff.
///
/// A hot upgrade deliberately closes every old stream after the replacement
/// listener owns the socket. The reader therefore returns normally from the
/// user's perspective, but it must not be the lifetime of the window's IPC
/// thread: the next dial repeats the protocol handshake and lets `SessionList`
/// rebuild the existing topology. LAN and tailnet dials retain their explicit
/// one-shot failure contract, because retrying a rejected peer would leave a
/// window indefinitely aimed at the wrong machine.
// @lat: [[client#GPUI Client Spike#Session Lifecycle#Server upgrade reconnect]]
async fn supervise_connection(mut ctx: IpcThread) {
    const INITIAL_RECONNECT_DELAY: Duration = Duration::from_millis(100);
    const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(2);

    let retry_local =
        lan_dial::target_from_env().is_none() && remote_handshake::target_from_env().is_none();
    let mut reconnect_delay = INITIAL_RECONNECT_DELAY;
    let Some(in_rx) = ctx.in_rx.take() else {
        tracing::error!("IPC inbound receiver was already consumed");
        set_status(
            &ctx.shared.status,
            &ctx.shared.generation,
            "IPC inbound receiver unavailable".to_owned(),
        );
        return;
    };
    spawn_drain(
        in_rx,
        Arc::clone(&ctx.shared.panes),
        Arc::clone(&ctx.shared.generation),
        Arc::clone(&ctx.shared.prompt_marks),
    );

    loop {
        match run_connection(&mut ctx).await {
            Ok(()) => tracing::info!("server connection closed"),
            Err(error) => {
                // Logged as well as published: the status line is one line wide
                // and the reason a connect failed — a stale socket, a refused
                // autostart, a rejected dial — is the first thing anyone
                // diagnosing a dead window needs, long after the window is gone.
                tracing::warn!(%error, "server connection failed");
                if !retry_local {
                    ctx.shared.connected.store(false, Ordering::Release);
                    set_status(
                        &ctx.shared.status,
                        &ctx.shared.generation,
                        format!("server connection failed: {error}"),
                    );
                    return;
                }
            }
        }

        if !retry_local {
            return;
        }

        ctx.shared.connected.store(false, Ordering::Release);
        set_status(
            &ctx.shared.status,
            &ctx.shared.generation,
            format!("server connection lost; retrying in {} ms", reconnect_delay.as_millis()),
        );
        tokio::time::sleep(reconnect_delay).await;
        reconnect_delay = (reconnect_delay * 2).min(MAX_RECONNECT_DELAY);
    }
}

/// Establish this client's one connection and serve it until it closes.
///
/// Three transports reach the same server-side protocol. The default is the
/// local Unix socket; `SCRIBE_LAN_DIAL` instead points the client at a peer on
/// the local network, which is reached over TCP + pinned mutual TLS and gated by
/// the owning side's device approval before a single byte of window state flows
/// (feature 014); `SCRIBE_REMOTE_DIAL` points it at a peer on the tailnet, which
/// is reached over plain TCP and gated by the mandatory `RemoteHandshake`
/// preamble (feature 013). Past those gates the streams are interchangeable, so
/// all three paths converge on [`serve_connection`].
///
/// LAN wins a double-set: it is the preferred transport for the same machine
/// (research C3), so a peer reachable both ways is dialed over the encrypted,
/// device-approved link rather than the tailnet.
async fn run_connection(ctx: &mut IpcThread) -> Result<(), String> {
    if let Some((host, port)) = lan_dial::target_from_env() {
        return run_lan_connection(ctx, host, port).await;
    }
    if let Some((host, port)) = remote_handshake::target_from_env() {
        return run_remote_connection(ctx, host, port).await;
    }
    run_local_connection(ctx).await
}

/// Connect to this machine's own server over the local Unix socket.
///
/// Both local-only environments are probed FIRST, before the session connection
/// exists. `GetLanEnv` and `GetRemoteEnv` are pre-`Hello` first frames the server
/// answers on their own transient sockets, so they have to be separate
/// connections either way; running them up front means the window has both
/// summaries before the first frame paints, and it leaves the session connection
/// as the last socket this process opened, which is what the E2E wire tap
/// addresses the client by.
async fn run_local_connection(ctx: &mut IpcThread) -> Result<(), String> {
    let probes = Box::new(LocalProbes {
        lan_env: probe_lan_env().await,
        remote_env: probe_remote_env().await,
    });
    // Autostart, not a bare connect: a client launched from a desktop entry is
    // routinely the first thing to want a server, so a refused socket starts the
    // per-user service and waits for it rather than failing the window. The
    // refusal is diagnosed on the way through, which is what turns a leftover
    // socket file from "connection refused" into a named stale socket.
    let socket_path = server_socket_path();
    let stream = server_lifecycle::connect_or_start_server(&socket_path)
        .await
        .map_err(|error| error.to_string())?;
    // Connected — but possibly to a server older than the installed binary
    // (a package upgrade or a local rebuild landed under a live process). Say so
    // on the status bar instead of leaving the mismatch to surface as a protocol
    // oddity later.
    if let Some(reason) = server_lifecycle::connected_server_staleness(&stream) {
        set_status(
            &ctx.shared.status,
            &ctx.shared.generation,
            format!("stale scribe-server: {reason}"),
        );
    }
    // Socket is up: light the status-bar connection dot green.
    ctx.shared.connected.store(true, Ordering::Release);
    let (reader, writer) = stream.into_split();
    serve_connection(ctx, reader, writer, Transport::Local(probes)).await
}

/// Dial a tailnet peer over plain TCP, run the mandatory `RemoteHandshake`
/// preamble, and only then serve the connection.
///
/// Every failure short of acceptance is terminal for this process, for the same
/// reason the LAN dial's is: the client was launched to control that peer, so
/// falling back to the local server would silently attach the user to the wrong
/// machine. The typed refusal is published to the window before the process
/// gives up, so the last thing on screen names why (UX-002).
async fn run_remote_connection(ctx: &mut IpcThread, host: String, port: u16) -> Result<(), String> {
    let remote = Arc::clone(&ctx.shared.remote);
    let status = Arc::clone(&ctx.shared.status);
    let generation = Arc::clone(&ctx.shared.generation);
    tracing::info!(%host, port, "dialing a tailnet peer instead of the local socket");

    let dialer = RemoteDialer::new(host, port);
    let mut stream = dialer.connect().await.map_err(|error| error.to_string())?;
    let outcome = remote_handshake::perform_remote_handshake(
        &mut stream,
        remote_handshake::local_device_name(),
    )
    .await;
    publish_remote_status(&remote, &status, &generation, |chrome| chrome.settle_dial(outcome));
    if outcome != RemoteConnectOutcome::Accepted {
        let (peer_host, peer_port) = dialer.target();
        return Err(format!("tailnet dial to {peer_host}:{peer_port} was not accepted"));
    }

    // Accepted: the tailnet link now behaves exactly like the local socket, and
    // the claim the picker asked for rides the ordinary `Hello`.
    ctx.shared.connected.store(true, Ordering::Release);
    let (reader, writer) = tokio::io::split(stream);
    serve_connection(
        ctx,
        reader,
        writer,
        Transport::Remote {
            window_id: remote_handshake::remote_dial_window_from_env(),
            takeover: remote_handshake::remote_dial_takeover_from_env(),
        },
    )
    .await
}

/// Apply `mutate` to the tailnet chrome and mirror the resulting summary onto
/// the status bar.
///
/// The dial path runs before any [`ReaderCtx`] exists, so it holds the three
/// shared handles directly rather than going through [`update_remote_chrome`].
fn publish_remote_status(
    remote: &Arc<Mutex<RemoteChrome>>,
    status: &Arc<Mutex<String>>,
    generation: &Arc<AtomicU64>,
    mutate: impl FnOnce(&mut RemoteChrome),
) {
    let Ok(mut guard) = remote.lock() else {
        tracing::warn!("remote chrome mutex poisoned; dropping dial update");
        return;
    };
    mutate(&mut guard);
    let line = guard.status_line();
    drop(guard);
    if let Some(line) = line {
        set_status(status, generation, line);
    }
}

/// Dial a LAN peer over TCP + pinned mutual TLS, run the `LanHello` preamble and
/// the owning side's device-approval gate, and only then serve the connection.
///
/// Every failure short of acceptance is terminal for this process: the client
/// was launched to control that peer, so falling back to the local server would
/// silently attach the user to the wrong machine.
async fn run_lan_connection(ctx: &mut IpcThread, host: String, port: u16) -> Result<(), String> {
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

/// The pre-`Hello` environment answers a LOCAL connection carries in from its
/// transient probe sockets. Each is `None` when the matching surface is disabled
/// in config or the probe failed, and the raw [`ServerMessage`] is kept so the
/// reply folds through the same handler the live reader uses.
struct LocalProbes {
    /// The feature-014 `LanEnv` reply, when `remote.lan.enabled`.
    lan_env: Option<ServerMessage>,
    /// The feature-013 `RemoteEnv` reply, when `remote.enabled`.
    remote_env: Option<ServerMessage>,
}

/// Which transport carried this connection, gating the local-only LAN/tailnet
/// queries and naming the window claim the `Hello` makes. A remote peer must
/// never be asked to enumerate this machine's LAN or tailnet view — the server
/// refuses it, and asking would put a pointless frame on the wire.
enum Transport {
    /// This machine's own server over the Unix socket, carrying the environment
    /// answers probed before the connection was opened. Boxed because a raw
    /// [`ServerMessage`] is far larger than the other variants' payloads and this
    /// value is moved once per process.
    Local(Box<LocalProbes>),
    /// A LAN peer over mutual TLS.
    Lan,
    /// A tailnet peer over plain TCP, past an accepted `RemoteHandshake`.
    Remote {
        /// The window to claim on the peer; `None` opens a fresh one.
        window_id: Option<scribe_common::ids::WindowId>,
        /// Whether this is the explicit-attach path that may displace a
        /// connected controller (FR-011); never set on auto-reconnect.
        takeover: bool,
    },
}

/// Send the handshake, start the writer and the drain, run the local-only LAN
/// startup probes, and hand the read half to the live reader.
///
/// Shared by both transports so the LAN path can never drift from the local one
/// in what it announces or which state it wires up.
async fn serve_connection<R, W>(
    ctx: &mut IpcThread,
    reader: R,
    mut writer: W,
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
    // opening an empty window of its own. A tailnet dial instead carries the
    // claim the connect picker made, including the explicit-attach takeover that
    // may displace a connected controller.
    let (claim_window, takeover) = match &transport {
        Transport::Local(_) | Transport::Lan => {
            (scribe_client::share_join::join_window_from_env(), false)
        }
        Transport::Remote { window_id, takeover } => (*window_id, *takeover),
    };
    if let Some(window_id) = claim_window {
        tracing::info!(%window_id, takeover, "claiming an existing window");
    }
    // Write the connection handshake directly before draining the shared
    // outbound queue. A reconnect may have queued UI work while no stream was
    // alive; allowing that work ahead of `Hello` would violate the protocol.
    write_message(
        &mut writer,
        &ClientMessage::Hello {
            window_id: claim_window,
            // Spec 010 C7: this client owns a host clipboard and a confirmation
            // modal, so it opts into OSC 52 gating. With the bit clear the
            // server takes the headless deny path and never sends a single
            // Clipboard* frame, which is what made the whole surface dead.
            clipboard_gating: true,
            takeover,
        },
    )
    .await
    .map_err(|error| error.to_string())?;
    write_message(&mut writer, &ClientMessage::ListSessions)
        .await
        .map_err(|error| error.to_string())?;

    let reader_ctx = reader_ctx(ctx);
    if let Transport::Local(probes) = transport {
        adopt_lan_surface(&reader_ctx, probes.lan_env);
        adopt_remote_surface(&reader_ctx, probes.remote_env);
    }
    tokio::select! {
        result = run_reader(reader, reader_ctx) => result,
        result = run_writer(writer, &mut ctx.out_rx) => result,
    }
}

/// Clone the durable per-window handles for one connection reader.
///
/// The supervisor keeps the channels themselves so a writer that stops during
/// an upgrade can resume draining the same ordered queue after the redial.
fn reader_ctx(ctx: &IpcThread) -> ReaderCtx {
    ReaderCtx {
        panes: Arc::clone(&ctx.shared.panes),
        attached: Arc::clone(&ctx.shared.attached),
        status: Arc::clone(&ctx.shared.status),
        generation: Arc::clone(&ctx.shared.generation),
        active_session: Arc::clone(&ctx.shared.active_session),
        ai: Arc::clone(&ctx.shared.ai),
        chrome_metadata: Arc::clone(&ctx.shared.chrome_metadata),
        tabs: Arc::clone(&ctx.shared.tabs),
        session_list_seen: Arc::clone(&ctx.shared.session_list_seen),
        share: Arc::clone(&ctx.shared.share),
        update: Arc::clone(&ctx.shared.update),
        lifecycle: Arc::clone(&ctx.shared.lifecycle),
        bells: Arc::clone(&ctx.shared.bells),
        ai_notices: Arc::clone(&ctx.shared.ai_notices),
        find: Arc::clone(&ctx.shared.find),
        lan: Arc::clone(&ctx.shared.lan),
        prompt_marks: Arc::clone(&ctx.shared.prompt_marks),
        workspaces: Arc::clone(&ctx.shared.workspaces),
        notes: Arc::clone(&ctx.shared.notes),
        clipboard: Arc::clone(&ctx.shared.clipboard),
        remote: Arc::clone(&ctx.shared.remote),
        out_tx: ctx.out_tx.clone(),
        in_tx: ctx.in_tx.clone(),
        sink: ctx.sink.clone(),
        size: ctx.size,
    }
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

/// Ask this machine's own server for its tailnet environment on a transient
/// socket.
///
/// Gated on `remote.enabled` exactly as the window-list poll is: with remote
/// access off there is no account to report and no peer a picker could offer, so
/// the frame would be pure noise. The raw reply travels back so it can be folded
/// through the same [`on_remote_message`] the live reader uses rather than a
/// second, divergent parser.
async fn probe_remote_env() -> Option<ServerMessage> {
    if !load_config().is_ok_and(|config| config.remote.enabled) {
        return None;
    }
    match remote_handshake::probe_remote_env().await {
        Ok(message) => Some(message),
        Err(error) => {
            tracing::warn!(%error, "remote environment probe failed");
            None
        }
    }
}

/// Fold the pre-probed `RemoteEnv` and ask the session connection for the
/// same-account tailnet peer list, which the server answers only for a LOCAL
/// connection.
fn adopt_remote_surface(ctx: &ReaderCtx, remote_env: Option<ServerMessage>) {
    let Some(remote_env) = remote_env else {
        return;
    };
    on_remote_message(ctx, remote_env);
    if let Err(error) = ctx.sink.list_remote_peers() {
        tracing::warn!(%error, "remote peer list request dropped: IPC writer closed");
    }
}

/// Drains the IPC-writer channel to the socket in FIFO order.
async fn run_writer<W>(
    mut writer: W,
    out_rx: &mut UnboundedReceiver<ClientMessage>,
) -> Result<(), String>
where
    W: AsyncWriteExt + Unpin,
{
    while let Some(message) = out_rx.recv().await {
        if let Err(error) = write_message(&mut writer, &message).await {
            return Err(format!("IPC writer stopped: {error}"));
        }
    }
    Err("IPC writer channel closed".to_owned())
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
    prompt_marks: Arc<Mutex<PromptMarks>>,
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
            for (session, op) in batch.iter() {
                // Each pane advances its own grid, so a background pane's burst
                // can never land in the focused pane's scrollback.
                let queue = session_queues.entry(session).or_default();
                let grid = grids.grid_mut(session);
                redraws += apply_pane_op(op, session, queue, grid, &prompt_marks);
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

/// Apply one drained operation to a pane, reporting how many repaints it owes.
///
/// The three arms share the grid because they are ordered against each other:
/// output advances it, a prompt mark reads the row the output left the cursor
/// on, and the suppressed-ED-3 snap resets the offset the output scrolled.
fn apply_pane_op(
    op: &PaneOp,
    session: SessionId,
    queue: &mut SyncFrameQueue,
    grid: &mut DisplayOnlyTerminal,
    prompt_marks: &Arc<Mutex<PromptMarks>>,
) -> usize {
    match op {
        PaneOp::Output(bytes) => {
            queue.queue_output_frames(bytes);
            usize::from(drain_all_committed(queue, grid).needs_redraw)
        }
        PaneOp::PromptMark { kind, exit_code } => {
            apply_prompt_mark(prompt_marks, session, *kind, *exit_code, grid);
            0
        }
        PaneOp::ScrollBottom => {
            // A real ED 3 resets the display offset inside `clear_history`; the
            // server stripped the sequence, so the snap is replayed here.
            grid.set_split_scroll_eligibility(SplitScrollEligibility::default());
            let moved = grid.scroll(Scroll::Bottom);
            tracing::info!(%session, moved, "server snapped the pane to the live bottom");
            usize::from(moved)
        }
        PaneOp::TrimScrollback { kept_rows } => apply_trim_scrollback(
            prompt_marks,
            session,
            grid.trim_history(*kept_rows),
            *kept_rows,
            grid,
        ),
    }
}

/// Shift a pane's absolute-row anchors past the scrollback rows the trim just
/// dropped, returning whether the pane needs a redraw.
///
/// The drop count comes from the *client's* grid rather than from the server's
/// reported history, because they are two different rings: the server names the
/// size it kept, and only the display grid knows how many of its own oldest
/// rows that removed. Marks anchored inside the dropped region are retired by
/// [`PromptMarks::on_trim`] — their rows no longer exist to jump to or tick.
fn apply_trim_scrollback(
    prompt_marks: &Arc<Mutex<PromptMarks>>,
    session: SessionId,
    dropped: usize,
    kept_rows: usize,
    grid: &DisplayOnlyTerminal,
) -> usize {
    if dropped == 0 {
        tracing::debug!(%session, kept_rows, "scrollback trim dropped no rows");
        return 0;
    }
    let Ok(mut marks) = prompt_marks.lock() else {
        tracing::warn!("prompt-mark mutex poisoned; dropping a scrollback trim");
        return 1;
    };
    marks.on_trim(session, dropped);
    tracing::info!(
        %session,
        dropped,
        kept_rows,
        history = grid.history_size(),
        marks = marks.marks(session).len(),
        "trimmed scrollback marks"
    );
    1
}

/// Anchor one OSC 133 mark against the pane grid the drain has just written to.
///
/// The anchor is read here, not in the reader, because absolute row positions
/// only mean anything once the output that moved the cursor has been applied —
/// and the drain is the only place that has both the grid and the batch order.
fn apply_prompt_mark(
    prompt_marks: &Arc<Mutex<PromptMarks>>,
    session: SessionId,
    kind: PromptMarkKind,
    exit_code: Option<i32>,
    grid: &DisplayOnlyTerminal,
) {
    let Ok(mut marks) = prompt_marks.lock() else {
        tracing::warn!("prompt-mark mutex poisoned; dropping mark");
        return;
    };
    let anchor = grid.prompt_anchor();
    let total = marks.record(session, kind, exit_code, anchor);
    tracing::info!(
        %session,
        ?kind,
        ?exit_code,
        history = anchor.history,
        cursor_row = anchor.cursor_row,
        marks = total,
        "prompt mark recorded"
    );
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
    /// Latched by the first `SessionList`; see the `session_list_seen` field of
    /// `Shared`.
    session_list_seen: Arc<AtomicBool>,
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
    /// AI transitions queued for the foreground's notification gate; see the
    /// `ai_notices` field of `Shared`.
    ai_notices: Arc<Mutex<Vec<AiNotice>>>,
    /// Latest `SearchResults` reply the find overlay renders and highlights.
    find: Arc<Mutex<FindResults>>,
    /// Feature-014 LAN state the reader parks approval requests in and folds the
    /// peer-list, environment, and dial-gate answers onto.
    lan: Arc<Mutex<LanChrome>>,
    /// Per-session OSC 133 command records. The reader shifts them on a
    /// `TrimScrollback`; the drain anchors new marks into them.
    prompt_marks: Arc<Mutex<PromptMarks>>,
    /// `WorkspaceInfo` answers the reader parks for the shell's reconcile pass;
    /// see the `workspaces` field of `Shared`.
    workspaces: Arc<Mutex<Vec<WorkspaceInfo>>>,
    /// Server-owned workspace notes the reader folds snapshots and change
    /// broadcasts into; see the `notes` field of `Shared`.
    notes: Arc<Mutex<WorkspaceNotesStore>>,
    /// Spec-010 OSC 52 state the reader records gating on, parks confirmation
    /// requests in, and queues host clipboard jobs onto.
    clipboard: Arc<Mutex<ClipboardBridge>>,
    /// Feature-013 tailnet state the reader folds every remote answer onto and
    /// queues inbound automation actions in.
    remote: Arc<Mutex<RemoteChrome>>,
    out_tx: UnboundedSender<ClientMessage>,
    in_tx: UnboundedSender<InboundEvent>,
    sink: IpcSink,
    size: TerminalSize,
}

/// Park a clicked notification's session for the foreground's lifecycle tick.
///
/// Free-standing because it runs on the dispatcher's own relay thread, which
/// holds no view and no GPUI context — only the shared queue.
fn record_notification_click(clicks: &Mutex<Vec<SessionId>>, session_id: SessionId) {
    if let Ok(mut queue) = clicks.lock() {
        queue.push(session_id);
    } else {
        tracing::warn!("notification click queue poisoned; dropping a click");
    }
}

/// Queue one AI transition for the foreground's notification gate.
///
/// The reader deliberately makes no decision here: whether a transition
/// notifies depends on the live `[notifications]` config and the window's focus
/// state, neither of which this thread can see, and delivery needs the
/// dispatcher handle the view owns. A poisoned queue costs notifications, never
/// the pane's output stream.
fn queue_ai_notice(ctx: &ReaderCtx, notice: AiNotice) {
    if let Ok(mut queue) = ctx.ai_notices.lock() {
        queue.push(notice);
    } else {
        tracing::warn!("AI notice queue mutex poisoned; dropping notice");
    }
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

/// Fold one AI notice onto the shared [`AiChrome`].
///
/// Extracted from [`dispatch_server_message`] so that table stays a screen of
/// routing decisions. The caller only ever hands over the three variants named
/// here; anything else is a routing bug and is reported as an unhandled
/// message rather than silently ignored.
fn on_ai_message(ctx: &ReaderCtx, message: ServerMessage) {
    match message {
        // The server has already merged partial OSC events onto the stored
        // state, so the percent that arrives here is the live one.
        ServerMessage::AiStateChanged { session_id, ai_state } => {
            queue_ai_notice(
                ctx,
                AiNotice::StateChanged { session_id, state: ai_state.state.clone() },
            );
            update_ai_chrome(ctx, |ai| {
                let provider = ai_state.provider;
                ai.tracker.update(session_id, ai_state);
                ai.tracker.remember_provider(session_id, provider);
            });
        }
        ServerMessage::AiStateCleared { session_id } => {
            queue_ai_notice(ctx, AiNotice::Cleared { session_id });
            update_ai_chrome(ctx, |ai| {
                ai.tracker.remove(session_id);
                ai.tracker.clear_context(session_id);
            });
        }
        ServerMessage::PromptReceived { session_id, text, .. } => {
            let at = std::time::SystemTime::now();
            update_ai_chrome(ctx, |ai| ai.record_prompt(session_id, text, at));
        }
        other => unhandled_server_message(&other),
    }
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

/// Apply `mutate` to the shared tailnet chrome and request a repaint.
///
/// Poisoning is dropped silently for the same reason it is on the AI chrome:
/// losing one remote update must never tear the reader down and with it the
/// pane's terminal output.
fn update_remote_chrome(ctx: &ReaderCtx, mutate: impl FnOnce(&mut RemoteChrome)) {
    let Ok(mut guard) = ctx.remote.lock() else {
        tracing::warn!("remote chrome mutex poisoned; dropping update");
        return;
    };
    mutate(&mut guard);
    drop(guard);
    ctx.generation.fetch_add(1, Ordering::Release);
}

/// Fold a tailnet change and mirror the resulting one-line summary onto the
/// status bar, so every remote transition the reader applies is also visible on
/// screen.
fn update_remote_chrome_and_status(ctx: &ReaderCtx, mutate: impl FnOnce(&mut RemoteChrome)) {
    update_remote_chrome(ctx, mutate);
    let line = ctx.remote.lock().ok().and_then(|remote| remote.status_line());
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

/// Name, count, and warn about an inbound message the live reader drops.
fn unhandled_server_message(message: &ServerMessage) {
    let dropped = UNHANDLED_SERVER_MESSAGES.fetch_add(1, Ordering::Relaxed) + 1;
    tracing::warn!(
        variant = server_message_variant(message),
        dropped,
        "server message not wired into the GPUI client"
    );
}

/// Running total of overlay-originated actions that reached a dispatcher with
/// no live handler behind it.
///
/// Distinct from [`UNHANDLED_SERVER_MESSAGES`]: those are inbound frames the
/// reader dropped, these are palette rows, context-menu rows, and key actions
/// whose destination surface (the settings window, the remote picker, a
/// server-side select-all) is not ported yet. Both are user-invisible without a
/// log line, which is why every drop is named and counted here.
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

/// Paintable AI border strips inset into one pane's local coordinate space.
fn ai_pane_border(rect: Rect, color: gpui::Rgba) -> Vec<gpui::AnyElement> {
    let local = Rect { x: 0.0, y: 0.0, width: rect.width, height: rect.height };
    pane_border_edges(local, 0.0)
        .into_iter()
        .map(|edge| {
            div()
                .absolute()
                .left(px(edge.x))
                .top(px(edge.y))
                .w(px(edge.width))
                .h(px(edge.height))
                .bg(color)
                .into_any_element()
        })
        .collect()
}

/// Return the context suffix for one tab from its latest independent AI state.
fn tab_context_suffix_for(
    tracker: &AiStateTracker,
    session_id: SessionId,
    thresholds: &AiContextThresholds,
) -> Option<scribe_client::tab_bar::ContextSuffix> {
    let percent = tracker.context_for(session_id)?;
    context_suffix(
        percent,
        thresholds.warn,
        thresholds.danger,
        tracker.context_suffix_suppressed(session_id),
    )
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
    // A toast outliving the pane it points at is a click that can only land
    // nowhere, so the exit retires it on the same path that forgets the chrome.
    queue_ai_notice(ctx, AiNotice::Cleared { session_id });
    // The pane that showed it is retired by the view's reconcile pass; the
    // grid and the output gate are dropped here so a recycled session id can
    // never inherit the dead pane's scrollback.
    if let Ok(mut streaming) = ctx.attached.lock() {
        streaming.remove(&session_id);
    }
    if let Ok(mut grids) = ctx.panes.lock() {
        grids.forget(session_id);
    }
    if let Ok(mut marks) = ctx.prompt_marks.lock() {
        marks.forget(session_id);
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
    let mut first_session_list = true;
    loop {
        let message: ServerMessage =
            read_message(&mut reader).await.map_err(|error| error.to_string())?;
        let is_session_list = matches!(&message, ServerMessage::SessionList { .. });
        // The focused tab is shared state: the view moves it for `next_tab` /
        // `select_tab_N`, so output gating reads it fresh on every message
        // rather than caching a local `attached`.
        let attached = ctx.active_session.lock().ok().and_then(|guard| *guard);
        dispatch_server_message(message, &ctx, &mut registry, attached, first_session_list)?;
        if is_session_list {
            first_session_list = false;
        }
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
    first_session_list: bool,
) -> Result<(), String> {
    match message {
        ServerMessage::Welcome { window_id, participant_id, clipboard_gating, .. } => {
            on_welcome(ctx, registry, window_id, participant_id, clipboard_gating);
        }
        ServerMessage::SessionList { sessions, workspaces, .. } => {
            on_session_list(ctx, registry, &sessions, &workspaces, first_session_list)?;
        }
        ServerMessage::SessionCreated { session_id, workspace_id, shell_name } => {
            registry.on_session_created(session_id, workspace_id);
            open_created_tab(ctx, session_id, workspace_id, shell_name)?;
        }
        ServerMessage::SessionExited { session_id, .. } => {
            on_session_exited(ctx, registry, session_id, attached)?;
        }
        // The three AI notices all land in the one shared [`AiChrome`] the
        // status bar and the prompt bar render from, so they are named here and
        // routed as one to [`on_ai_message`].
        ai @ (ServerMessage::AiStateChanged { .. }
        | ServerMessage::AiStateCleared { .. }
        | ServerMessage::PromptReceived { .. }) => on_ai_message(ctx, ai),
        // OSC 133 marks, the suppressed-ED-3 snap and the AI scrollback trim are
        // all positional: each describes the grid *after* output the server has
        // already sent, so they are routed as one onto the same ordered inbound
        // channel as that output rather than applied here.
        positional @ (ServerMessage::PromptMark { .. }
        | ServerMessage::ScrollBottom { .. }
        | ServerMessage::TrimScrollback { .. }) => {
            on_positional_pane_message(ctx, positional);
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
        info @ ServerMessage::WorkspaceInfo { .. } => on_workspace_info(ctx, info),
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
        // The snapshot answers a `WorkspaceNotesGet`; the change broadcast is
        // pushed to every connected window after one accepted mutation. Both
        // carry the same server-owned collection shape and both land in the one
        // cache the open modal reads, so they are named here and routed as one
        // to [`on_workspace_notes_message`].
        notes @ (ServerMessage::WorkspaceNotesSnapshot { .. }
        | ServerMessage::WorkspaceNotesChanged { .. }) => on_workspace_notes_message(ctx, notes),
        ServerMessage::Error { message } => on_server_error(ctx, message),
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
        // Spec 010: the three OSC 52 frames land in one shared [`ClipboardBridge`]
        // the GPUI thread drains, so they route as one to [`on_clipboard_message`].
        clipboard @ (ServerMessage::ClipboardPromptRequest { .. }
        | ServerMessage::ClipboardBridgeWrite { .. }
        | ServerMessage::ClipboardBridgeReadRequest { .. }) => {
            on_clipboard_message(ctx, clipboard);
        }
        // Feature 013: every tailnet answer — the dial preamble's reply, the
        // discovery/environment replies the startup probe asks for, the
        // displacement and severance notices, and the automation round trip —
        // lands in the one shared [`RemoteChrome`], so they are named here and
        // routed as one to [`on_remote_message`]. Naming them is what keeps the
        // remote surface reachable: unnamed, a `WindowTakenOver` reaches the drop
        // counter and the user keeps typing into a window someone else is
        // driving.
        remote @ (ServerMessage::RemoteHandshakeReply { .. }
        | ServerMessage::RemotePeerList { .. }
        | ServerMessage::RemoteEnv { .. }
        | ServerMessage::RemoteDisconnect { .. }
        | ServerMessage::WindowTakenOver { .. }
        | ServerMessage::RunAction { .. }
        | ServerMessage::ActionDispatched { .. }) => on_remote_message(ctx, remote),
        other => unhandled_server_message(&other),
    }
    Ok(())
}

/// Adopt the server's session inventory: rebuild the reconnect topology, seed
/// the chrome metadata, and reconcile the tab strip.
///
/// The list replays each pane's last-known CWD, branch and context, so a
/// reattach restores the status bar without waiting for the next shell prompt
/// to re-emit them.
fn on_session_list(
    ctx: &ReaderCtx,
    registry: &mut session_lifecycle::SessionRegistry,
    sessions: &[SessionInfo],
    workspaces: &[scribe_common::protocol::WorkspaceListEntry],
    first_on_connection: bool,
) -> Result<(), String> {
    registry.rebuild_from_session_list(sessions);
    // Latched before anything else folds the list in: the cold-restart replay
    // reads this to tell "the server has nothing" from "the server has not
    // answered yet", and only the former may replay a persisted snapshot.
    ctx.session_list_seen.store(true, Ordering::Release);
    tracing::info!(
        sessions = registry.len(),
        workspaces = registry.reconnect_topology().len(),
        "rebuilt reconnect topology"
    );
    update_chrome_metadata(ctx, |metadata| {
        metadata.seed_from_session_list(sessions, workspaces);
    });
    if first_on_connection {
        reattach_visible_sessions(ctx, sessions)?;
    }
    let attached = ctx.active_session.lock().ok().and_then(|guard| *guard);
    sync_tab_strip(ctx, sessions, attached)
}

/// Reattach panes retained by the live window to a replacement server stream.
///
/// The local `attached` set describes which sessions the window still shows,
/// but an `AttachSessions` grant belongs to one IPC connection. A server
/// handoff therefore needs to replay the set after the replacement connection's
/// first `SessionList`. Dimensions come from each pane's existing display grid
/// so split panes keep their geometry across the handoff.
fn reattach_visible_sessions(ctx: &ReaderCtx, sessions: &[SessionInfo]) -> Result<(), String> {
    let live: HashSet<_> = sessions.iter().map(|session| session.session_id).collect();
    let retained = ctx.attached.lock().map_or_else(
        |_| Vec::new(),
        |mut attached| {
            attached.retain(|session_id| live.contains(session_id));
            sessions
                .iter()
                .map(|session| session.session_id)
                .filter(|session_id| attached.contains(session_id))
                .collect::<Vec<_>>()
        },
    );
    if retained.is_empty() {
        return Ok(());
    }

    let dimensions = ctx.panes.lock().map_or_else(
        |_| vec![ctx.size; retained.len()],
        |panes| {
            retained
                .iter()
                .map(|session_id| {
                    let Some((cols, rows)) = panes.dimensions(*session_id) else {
                        return ctx.size;
                    };
                    TerminalSize {
                        cols: u16::try_from(cols).unwrap_or(ctx.size.cols),
                        rows: u16::try_from(rows).unwrap_or(ctx.size.rows),
                        cell_width: ctx.size.cell_width,
                        cell_height: ctx.size.cell_height,
                    }
                })
                .collect()
        },
    );

    tracing::info!(sessions = retained.len(), "reattaching visible sessions");
    for session_id in &retained {
        tracing::info!(%session_id, "attaching to session");
    }
    ctx.out_tx
        .send(ClientMessage::AttachSessions {
            session_ids: retained.clone(),
            dimensions: dimensions.clone(),
        })
        .map_err(|_| "writer channel closed".to_owned())?;
    for (session_id, size) in retained.iter().zip(dimensions) {
        ctx.sink.resize(*session_id, size).map_err(|error| error.to_string())?;
    }
    ctx.sink.subscribe(retained).map_err(|error| error.to_string())
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

/// The prefix the server puts on a rejected workspace-notes mutation.
///
/// `ServerMessage::Error` is one flat channel for every rejection, so this is
/// what separates a notes rejection from an unrelated one and lets it surface
/// in the modal's own footer instead of only in the status line.
const WORKSPACE_NOTES_ERROR_PREFIX: &str = "workspace note mutation failed";

/// Fold one server notes answer into the client-side cache.
///
/// `WorkspaceNotesSnapshot` is the reply to the `WorkspaceNotesGet` the modal
/// sends when it opens; `WorkspaceNotesChanged` is what the server fans out to
/// every connected window after it has persisted an accepted mutation. The
/// broadcast is the *only* thing that moves the rendered lists — the modal
/// never optimistically applies its own edit — which is what keeps two windows
/// converged on the server's last accepted state.
///
/// The reader cannot touch the modal, which is a GPUI entity owned by the
/// window thread, so it writes the cache, bumps its version, and bumps the
/// repaint generation; [`TerminalView::sync_workspace_notes`] adopts it on the
/// next frame.
fn on_workspace_notes_message(ctx: &ReaderCtx, message: ServerMessage) {
    let Ok(mut store) = ctx.notes.lock() else {
        tracing::warn!("workspace notes mutex poisoned; dropping a notes answer");
        return;
    };
    match message {
        ServerMessage::WorkspaceNotesSnapshot { collections } => {
            tracing::info!(collections = collections.len(), "workspace notes snapshot received");
            store.apply_collections(collections);
        }
        ServerMessage::WorkspaceNotesChanged { collection } => {
            tracing::info!(
                workspace_id = %collection.workspace_id,
                active = collection.active_notes.len(),
                archived = collection.archived_notes.len(),
                "workspace notes changed",
            );
            store.apply_collection(collection);
        }
        // Unreachable: the caller only routes the two notes variants here.
        other => tracing::warn!(kind = server_message_variant(&other), "not a notes message"),
    }
    drop(store);
    ctx.generation.fetch_add(1, Ordering::Release);
}

/// Surface one server rejection: always on the status line, and additionally in
/// the notes modal's footer when the rejection is about a notes mutation.
fn on_server_error(ctx: &ReaderCtx, message: String) {
    if message.starts_with(WORKSPACE_NOTES_ERROR_PREFIX) {
        if let Ok(mut store) = ctx.notes.lock() {
            store.set_error(message.clone());
        } else {
            tracing::warn!("workspace notes mutex poisoned; dropping a notes error");
        }
    }
    set_status(&ctx.status, &ctx.generation, message);
}

/// Fold one `WorkspaceInfo` onto the window's chrome and park it for the shell.
///
/// This is the server's authoritative description of a workspace: its display
/// name, the accent colour from its rotating palette, and the project root it
/// was derived from. Two surfaces consume it and they live on different
/// threads, so it is split here rather than in the view. The name goes straight
/// into the shared [`ChromeMetadata`] the status bar renders its workspace
/// segment from — the same store `WorkspaceNamed` writes, so the two channels
/// cannot disagree. The accent and the id itself belong to a [`PaneShell`]
/// region, which is a GPUI entity the reader must never touch, so the whole
/// update is parked for the next reconcile pass.
///
/// Parking the id is what makes `ClientMessage::CreateWorkspace` complete: the
/// region a `workspace_split_*` opened is client-local until this reply re-keys
/// it onto the workspace the server actually minted.
fn on_workspace_info(ctx: &ReaderCtx, message: ServerMessage) {
    let ServerMessage::WorkspaceInfo { workspace_id, name, accent_color, project_root, .. } =
        message
    else {
        // Unreachable: the caller only routes `WorkspaceInfo` here.
        unhandled_server_message(&message);
        return;
    };
    let accent = scribe_common::theme::hex_to_rgba(&accent_color).ok();
    if accent.is_none() {
        tracing::warn!(%workspace_id, accent_color, "workspace accent is not a #rrggbb colour");
    }
    // An absent name is the server saying the workspace is outside every
    // configured root, which must clear a name the status bar is still showing.
    update_chrome_metadata(ctx, |store| {
        store.name_workspace(workspace_id, name.clone().unwrap_or_default());
    });
    let Ok(mut parked) = ctx.workspaces.lock() else {
        tracing::warn!("workspace info mutex poisoned; dropping WorkspaceInfo");
        return;
    };
    tracing::info!(%workspace_id, ?name, accent_color, "workspace info received");
    parked.push(WorkspaceInfo { workspace_id, name, accent, project_root });
    drop(parked);
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
                scribe_common::perf_probe::record_pty_output(session_id, data.len());
                update_ai_chrome(ctx, |ai| ai.tracker.note_activity(session_id));
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

/// Forward one positional pane event onto the ordered inbound channel.
///
/// A prompt mark's anchor row and a suppressed-ED-3 snap only mean anything
/// relative to the output around them, so both travel the same FIFO the pane's
/// bytes do and are applied by the drain, not here. Anything other than those
/// two variants is a routing error in [`dispatch_server_message`], not a
/// protocol event, so it is counted as unhandled rather than silently dropped.
fn on_positional_pane_message(ctx: &ReaderCtx, message: ServerMessage) {
    let (session_id, event) = match message {
        ServerMessage::PromptMark { session_id, kind, exit_code, .. } => {
            (session_id, InboundEvent::PromptMark { session_id, kind, exit_code })
        }
        ServerMessage::ScrollBottom { session_id } => {
            (session_id, InboundEvent::ScrollBottom { session_id })
        }
        ServerMessage::TrimScrollback { session_id, history_rows } => (
            session_id,
            InboundEvent::TrimScrollback {
                session_id,
                kept_rows: usize::try_from(history_rows).unwrap_or(usize::MAX),
            },
        ),
        other => {
            unhandled_server_message(&other);
            return;
        }
    };
    if is_attached(ctx, session_id) {
        forward_inbound(&ctx.in_tx, event);
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

/// Fold one feature-013 tailnet answer into the shared [`RemoteChrome`].
///
/// Seven variants, three jobs. The environment/peer pair is passive chrome. The
/// displacement pair is not: a `WindowTakenOver` freezes the window under the
/// reclaim banner and a `RemoteDisconnect` records why the peer severed the
/// link, and both must be visible before the user's next keystroke goes
/// somewhere it no longer belongs. The automation pair is the `scribe action …`
/// round trip: `RunAction` is queued for the foreground (the action it names may
/// only run on the thread that owns the window) and `ActionDispatched`
/// acknowledges a dispatch this client sent.
///
/// Anything other than the remote family is a programming error in
/// [`dispatch_server_message`]'s routing, not a protocol event, so it is counted
/// as unhandled rather than silently dropped.
fn on_remote_message(ctx: &ReaderCtx, message: ServerMessage) {
    match message {
        ServerMessage::RemoteEnv { account, tailscale_detected } => {
            tracing::info!(
                identified = account.is_some(),
                tailscale_detected,
                "server tailnet environment"
            );
            update_remote_chrome_and_status(ctx, |remote| {
                remote.set_env(RemoteEnvSummary { account, tailscale_detected });
            });
        }
        ServerMessage::RemotePeerList { peers } => {
            tracing::info!(count = peers.len(), "server tailnet peer list");
            update_remote_chrome_and_status(ctx, |remote| remote.set_peers(peers));
        }
        ServerMessage::RemoteHandshakeReply {
            accepted,
            refusal,
            server_remote_protocol_version,
            server_scribe_version,
        } => {
            // Normally consumed by `remote_handshake::perform_remote_handshake`
            // before this reader exists; one arriving afterwards still means the
            // same thing, so it settles the dial identically rather than being
            // dropped as out of sequence.
            tracing::info!(
                accepted,
                ?refusal,
                server_remote_protocol_version,
                %server_scribe_version,
                "remote handshake reply"
            );
            let outcome = remote_handshake_outcome(accepted, refusal);
            update_remote_chrome_and_status(ctx, |remote| remote.settle_dial(outcome));
        }
        ServerMessage::RemoteDisconnect { reason } => {
            // Best-effort final frame: the peer closes the link right after it,
            // so recording the typed reason here is the only chance the window
            // has to say why the connection went away instead of just dying.
            tracing::info!(?reason, "remote peer severed the connection");
            update_remote_chrome_and_status(ctx, |remote| remote.sever(reason));
        }
        ServerMessage::WindowTakenOver { device_name, login_name } => {
            // Logged because the banner is pixels only: a scripted E2E can then
            // prove the notice reached the chrome and not just the socket.
            tracing::info!(%device_name, %login_name, "window taken over by another controller");
            update_remote_chrome_and_status(ctx, |remote| {
                remote.displace(LostControlState::new(device_name, login_name));
            });
        }
        ServerMessage::RunAction { action } => {
            // Queued rather than executed: the action opens tabs, splits panes,
            // and moves focus, all of which are GPUI entities only the window's
            // own thread may touch. The lifecycle tick drains the queue.
            tracing::info!(?action, "automation action received");
            update_remote_chrome(ctx, |remote| remote.queue_action(action));
        }
        ServerMessage::ActionDispatched { window_id } => {
            // The ack for a `DispatchAction` this client sent. There is nothing
            // to render — the effect arrives separately as the `RunAction` the
            // server routed to the window's controller — so it is logged as the
            // routing confirmation it is.
            tracing::info!(%window_id, "automation action routed by the server");
        }
        // Unreachable: the caller only routes the remote family here.
        other => unhandled_server_message(&other),
    }
}

/// Map a decoded `RemoteHandshakeReply` onto the typed dial outcome. A refusal
/// with no reason is a protocol violation, reported as a generic connection
/// failure rather than inventing a cause — the same rule
/// [`remote_handshake::perform_remote_handshake`] applies during the preamble.
fn remote_handshake_outcome(
    accepted: bool,
    refusal: Option<scribe_common::protocol::RemoteRefusal>,
) -> RemoteConnectOutcome {
    match (accepted, refusal) {
        (true, _) => RemoteConnectOutcome::Accepted,
        (false, Some(reason)) => RemoteConnectOutcome::Refused(reason),
        (false, None) => RemoteConnectOutcome::ConnectionFailure,
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
    clipboard_gating: bool,
) {
    registry.adopt_window(window_id);
    update_lifecycle(ctx, |lifecycle| lifecycle.adopt_window(window_id));
    update_share_chrome(ctx, |share| share.set_self_id(participant_id));
    // Spec 010 C7: the server echoes back whether it will route OSC 52 through
    // this client. Recording it here is what lets the clipboard arms below
    // refuse to act on a frame that arrived without a negotiated capability.
    if let Ok(mut bridge) = ctx.clipboard.lock() {
        bridge.set_gating(clipboard_gating);
    }
    tracing::info!(
        adopted = ?registry.adopted_window(),
        ?participant_id,
        clipboard_gating,
        "welcome: adopted window"
    );
}

/// Fold one spec-010 OSC 52 frame into the shared [`ClipboardBridge`].
///
/// Nothing is performed here. The reader thread owns neither the modal (a GPUI
/// entity) nor the host clipboard (arboard plus the FR-019 focus gate, both
/// window-thread resources), so a prompt is parked and a bridge op is queued
/// for [`TerminalView::poll_clipboard`] to run on the next foreground tick.
/// Frames arriving before the `clipboard_gating` capability was negotiated are
/// dropped, matching the winit client: without that bit the server should not
/// have sent them, and acting on one would touch the host clipboard on the say
/// so of a peer that never agreed to gate.
fn on_clipboard_message(ctx: &ReaderCtx, message: ServerMessage) {
    let Ok(mut bridge) = ctx.clipboard.lock() else {
        tracing::warn!("clipboard bridge mutex poisoned; dropping the OSC 52 message");
        return;
    };
    if !bridge.gating() {
        tracing::debug!("OSC 52 message received before gating was negotiated; ignoring");
        return;
    }
    match message {
        ServerMessage::ClipboardPromptRequest {
            session_id,
            request_id,
            op,
            selection,
            preview,
        } => {
            tracing::info!(%session_id, ?request_id, ?op, ?selection, "OSC 52 prompt requested");
            bridge.park_prompt(ClipboardPrompt { request_id, op, selection, preview });
        }
        ServerMessage::ClipboardBridgeWrite { session_id, selection, payload } => {
            tracing::debug!(
                %session_id,
                ?selection,
                payload_len = payload.len(),
                "OSC 52 bridge write queued for the host clipboard",
            );
            if bridge.push_job(BridgeJob::Write { selection, payload }) {
                tracing::warn!("OSC 52 bridge queue full; dropped the oldest job");
            }
        }
        ServerMessage::ClipboardBridgeReadRequest { session_id, request_id, selection } => {
            tracing::debug!(
                %session_id,
                ?request_id,
                ?selection,
                "OSC 52 bridge read queued for the host clipboard",
            );
            if bridge.push_job(BridgeJob::Read { request_id, selection }) {
                tracing::warn!("OSC 52 bridge queue full; dropped the oldest job");
            }
        }
        other => unhandled_server_message(&other),
    }
    drop(bridge);
    ctx.generation.fetch_add(1, Ordering::Release);
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
    // The tab's label tracks the OSC 0/2 title once one arrives, so the shell a
    // pane actually runs is recorded separately — that, not the label, is what a
    // dropped file path has to be quoted for.
    update_chrome_metadata(ctx, |metadata| {
        metadata.set_shell_name(session_id, shell_name.clone());
    });
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
    forward_inbound(in_tx, InboundEvent::PaneOutput { session_id, bytes });
}

/// Hand one event to the coalescing drain, preserving arrival order.
///
/// Every pane-affecting message goes through here so output and the positional
/// events interleaved with it (prompt marks, the suppressed-ED-3 snap) cannot
/// be reordered relative to each other.
fn forward_inbound(in_tx: &UnboundedSender<InboundEvent>, event: InboundEvent) {
    if in_tx.send(event).is_err() {
        tracing::warn!("inbound drain closed; dropping pane event");
    }
}

fn set_status(status: &Arc<Mutex<String>>, generation: &AtomicU64, message: String) {
    if let Ok(mut status) = status.lock() {
        *status = message;
        generation.fetch_add(1, Ordering::Release);
    }
}

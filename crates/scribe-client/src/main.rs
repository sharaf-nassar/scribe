//! GPUI Scribe client over Scribe's frozen local IPC protocol.

mod ipc_bridge;
mod pane_shell;
mod session_lifecycle;
mod sync_frames;
mod terminal;
mod terminal_element;
mod terminal_image_renderer_probe;

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use gpui::{
    App, AppContext as _, AsyncApp, Bounds, Context, DragMoveEvent, ElementId, Entity, FocusHandle,
    Focusable, KeyDownEvent, ModifiersChangedEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, Point, Render, ScrollWheelEvent, Size, Subscription, Task,
    TitlebarOptions, WeakEntity, Window, WindowBackgroundAppearance, WindowBounds, WindowHandle,
    WindowOptions, canvas, div, prelude::*, px, relative, size,
};
use gpui_platform::application;
use scribe_client::ai_indicator::{AiStateTracker, pane_border_edges};
use scribe_client::animation::AnimationSettings;
use scribe_client::app_shortcuts::{self, CloseWindow, Quit};
use scribe_client::beads_board::{
    BEADS_BOARD_GRIP, BeadsBoardColors, BeadsBoardRender, BeadsBoards, HoverSource,
};
use scribe_client::beads_panel::{
    self, BeadsEditor, BeadsEditorKeyRoute, BeadsPanelRender, BeadsPanels, PanelWriteIntent,
};
use scribe_client::bell::{BellController, BellEvent};
use scribe_client::chrome_metadata::{ChromeMetadata, SessionChrome};
use scribe_client::ci_bar::{self, CiBarColors, CiBarModel, CiRunBars};
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
use scribe_client::gpui_image_lifecycle::GpuiImageCache;
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
use scribe_client::monitor;
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
    self, GridSize, ReplayLaunch, attach_dimensions_for_session, grid_for_rect, prepare_replay,
    round_positive_f32_to_u16,
};
use scribe_client::restore_state::{
    AiResumeMode, LaunchBinding, LaunchKind, RestoreStore, WindowRestoreState,
};
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
use scribe_client::terminal_image_scene::{
    CommittedImageScene, capability_mismatch_message, filter_terminal_image_placeholders,
};
use scribe_client::tooltip::{TooltipColors, TooltipPosition, TooltipRender, tooltip_element};
use scribe_client::update::UpdateState;
use scribe_client::url_detect::{self, SpanKind};
use scribe_client::vi_mode::ViMotion;
use scribe_client::window_chrome;
use scribe_client::window_lifecycle::{ExitReason, FocusReport, WindowLifecycle};
use scribe_client::window_state::{
    NIL_MONITOR_ID, ObservedWindowState, WindowGeometry, WindowRegistry, WindowState,
    clamp_geometry_to_layout, geometry_from_bounds, geometry_size_is_sane, logical_px_to_i32,
    normalize_legacy_geometry, window_bounds_for,
};
use scribe_client::workspace_layout::{self, WorkspaceDividerDrag};
use scribe_client::x11_focus::{X11FocusGuard, should_reconcile_window_activation};
use scribe_client::zoom::ZoomState;
use scribe_client::{
    smart_selection::CompiledSmartSelection,
    tab_bar::{
        GroupBadge, TabBarColors, TabData, accent_tab_tone, badge_label, context_suffix,
        flash_blend, px_units, reorder_target_index, tab_display_title,
    },
    tab_session::{TabAddress, TabEntry, TabSessions},
    titlebar::{
        TAB_MIN_WIDTH, TAB_WIDTH, TabActivationSource, TitlebarEvent, TitlebarView,
        beads_graph_icon, title_columns,
    },
};
use scribe_common::ai_state::{AiProvider, AiState};
use scribe_common::theme::ChromeColors;
use scribe_common::{
    config::{
        AiContextThresholds, PromptBarPosition, SmartSelectionActionKind, SmartSelectionConfig,
        StatusBarStatsConfig, TerminalPromptBarConfig, load_config,
    },
    framing::{read_message, write_message},
    ids::{SessionId, WindowId, WorkspaceId, new_launch_id},
    protocol::{
        AiLaunchSpec, AutomationAction, CiRunDetails, CiRunState, ClientMessage,
        ClipboardSelection, PromptMarkKind, ServerMessage, SessionInfo, TerminalSize,
        UpdateProgressState, WindowInfo, WorkspaceTreeNode,
    },
    screen::ScreenSnapshot,
    screen_replay::SessionReplay,
    socket::{ClientFocusGeneration, server_socket_path},
    terminal_images::ImageLimits,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{
        Notify,
        mpsc::{UnboundedSender, unbounded_channel},
    },
};

use crate::{
    ipc_bridge::{
        CoalescedBatch, InboundEvent, InboundReceiver, InboundSender, IpcSink,
        OUTBOUND_QUEUE_FRAMES, OutboundReceiver, OutboundSender, PaneOp, SessionLaunch, SinkError,
        WriteOutcome, inbound_channel, outbound_channel, run_drain, write_or_tear,
    },
    pane_shell::{ClosedPane, PanePlacement, PaneShell, WorkspaceInfo, WorkspaceInfoOutcome},
    session_lifecycle::{JumpDirection, PromptMarks},
    sync_frames::{present_next_burst, present_rebuild},
    terminal::{
        Content, DisplayOnlyTerminal, HoveredLink, PaneFrame, PaneGrid, PaneGrids, PaneStream,
        Scroll,
    },
    terminal_element::{
        CursorPaint, GridBounds, GridColors, GridFont, ImePaint, ScrollbarPaint, TerminalElement,
        TerminalImagesPaint, cell_at, hits_jump_chip, record_grid_area,
    },
};

/// Wall-clock origin captured at the very top of `main`, used to time
/// startup-to-first-frame for the perf A/B rig (`tools/perf-ab-rig`).
static PROCESS_START: OnceLock<Instant> = OnceLock::new();
static PROCESS_SHUTDOWN: OnceLock<Arc<ProcessShutdown>> = OnceLock::new();
thread_local! {
    /// Owner-local recency target for a duplicate launch's focus handoff.
    // ponytail: persist recency only if restore-child windows must win handoffs.
    static RECENT_TERMINAL_WINDOW: RefCell<Option<WindowHandle<TerminalView>>> = const {
        RefCell::new(None)
    };
    static TERMINAL_FOCUS_REPORTER: RefCell<Option<TerminalFocusReporter>> = const {
        RefCell::new(None)
    };
}

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

/// Width of the border a split window draws around every pane, in logical
/// pixels — `border_1()` in [`TerminalView::render_panes`].
///
/// GPUI hands taffy the border as a box inset, so the grid a bordered pane
/// paints is this much smaller on each side than the pane's placement rect.
/// [`TerminalView::painted_pane_rect`] takes it back off before the PTY is
/// told how many columns it has.
const PANE_BORDER_WIDTH: f32 = 1.0;

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
    /// Provider conversation id each session was last seen in, so a genuine
    /// conversation switch can retire the previous conversation's prompt
    /// history and context percent. Seeded by a cold-restart replay from the
    /// snapshot's launch record, because the resumed provider re-announces the
    /// id it was resumed with and that must not read as a switch.
    conversations: HashMap<SessionId, String>,
    /// Sessions whose provider explicitly exited. This is separate from
    /// provider absence: a fresh AI session has no state briefly and must not
    /// be demoted before its first hook event.
    binding_cleared: HashSet<SessionId>,
}

impl AiChrome {
    /// Build the chrome state from the per-state style config that governs
    /// which AI states are tracked at all.
    fn new(styles: scribe_common::config::AiStateStylesConfig) -> Self {
        Self {
            tracker: AiStateTracker::new(styles),
            prompts: HashMap::new(),
            conversations: HashMap::new(),
            binding_cleared: HashSet::new(),
        }
    }

    /// Record a prompt submission for `session_id`, seeding the first-prompt row
    /// and restarting the elapsed timer on the latest row.
    fn record_prompt(&mut self, session_id: SessionId, text: &str, at: std::time::SystemTime) {
        self.prompts.entry(session_id).or_default().prompts.record_prompt(text, at);
    }

    /// Fold one `AiStateChanged` edge onto the chrome: the conversation
    /// bookkeeping, the prompt bar's elapsed-timer freeze, and the tracker.
    ///
    /// One method rather than a closure inside the IPC reader so everything a
    /// state edge does to the chrome is exercisable without a live `ReaderCtx`
    /// — the freeze shipped broken precisely because only the pure formatter
    /// was under test and nothing asserted that anything ever set its input.
    fn apply_state_change(
        &mut self,
        session_id: SessionId,
        ai_state: scribe_common::ai_state::AiProcessState,
        at: std::time::SystemTime,
    ) {
        self.binding_cleared.remove(&session_id);
        let provider = ai_state.provider;
        // Before the tracker takes ownership: a state edge is the only frame
        // that names the provider's conversation, and a switch must take the
        // previous conversation's prompt bar with it.
        if let Some(conversation_id) = ai_state.conversation_id.as_deref() {
            self.note_conversation(session_id, conversation_id);
        }
        self.note_prompt_progress(session_id, &ai_state.state, at);
        self.tracker.update(session_id, ai_state);
        self.tracker.remember_provider(session_id, provider);
    }

    /// Freeze or resume the pane's elapsed timer for an AI state edge.
    ///
    /// Leaving `Processing` stamps the instant the timer freezes at, so the
    /// figure reads prompt-to-finish rather than wall-clock-since-prompt; a
    /// return to `Processing` clears the stamp and the timer ticks again, as
    /// does the next [`Self::record_prompt`]. The stamp is taken once per run
    /// rather than on every non-`Processing` edge, because an idle provider
    /// keeps emitting them and each one would push a frozen value forward.
    fn note_prompt_progress(
        &mut self,
        session_id: SessionId,
        state: &AiState,
        at: std::time::SystemTime,
    ) {
        if let Some(data) = self.prompts.get_mut(&session_id) {
            data.prompts.note_prompt_progress(state, at);
        }
    }

    /// Adopt a replayed pane's persisted prompt history, filed under the
    /// session the restored pane has just been given.
    ///
    /// Only a session that has said nothing yet is seeded: a `PromptReceived`
    /// that arrived before the pane adopted its session is newer than anything
    /// on disk, and the snapshot must not overwrite it.
    fn restore_prompts(
        &mut self,
        session_id: SessionId,
        prompts: PromptBarData,
        conversation_id: Option<String>,
    ) {
        if let Some(conversation_id) = conversation_id {
            self.conversations.insert(session_id, conversation_id);
        }
        self.prompts.entry(session_id).or_insert(prompts);
    }

    /// Adopt the server's view of every listed session's AI chrome.
    ///
    /// This is what makes a client restart against a surviving server
    /// non-destructive: the prompt bar, the indicator state, and the provider
    /// all come back from the `SessionList` reply instead of waiting for the
    /// provider's next hook event, which an idle conversation never sends.
    /// Prompt history goes through [`Self::restore_prompts`], so a
    /// `PromptReceived` that beat the list still wins.
    fn seed_from_session_list(&mut self, sessions: &[SessionInfo]) {
        for info in sessions {
            let session_id = info.session_id;
            if info.ai_state.is_some() || info.ai_provider_hint.is_some() {
                self.binding_cleared.remove(&session_id);
            }
            let conversation_id =
                info.ai_state.as_ref().and_then(|state| state.conversation_id.clone());
            if let Some(conversation_id) = conversation_id.as_deref() {
                self.note_conversation(session_id, conversation_id);
            }
            if let Some(prompts) = info.prompt_state.clone() {
                self.restore_prompts(session_id, prompts.into(), conversation_id);
            }
            if let Some(ai_state) = info.ai_state.clone() {
                self.tracker.update(session_id, ai_state);
            }
            // After `update`, which files the live state's own provider: the
            // hint is the fallback for a session whose visible state is gone
            // but whose provider-aware behaviour must survive the reattach.
            if let Some(provider) = info.ai_provider_hint {
                self.tracker.remember_provider(session_id, provider);
            }
        }
    }

    /// Note the conversation an AI state edge belongs to, retiring the session's
    /// prompt history when it names a *different* conversation than the last
    /// one seen.
    ///
    /// A first sighting is never a change: the history either belongs to this
    /// conversation or was seeded from the snapshot that resumed it. A new
    /// conversation also starts a fresh context window, so the previous
    /// percent must not bleed into the new bar.
    fn note_conversation(&mut self, session_id: SessionId, conversation_id: &str) {
        let previous = self.conversations.insert(session_id, conversation_id.to_owned());
        if previous.is_some_and(|old| old != conversation_id) {
            self.prompts.remove(&session_id);
            self.tracker.clear_context(session_id);
        }
    }

    /// The pane's prompt state, or `None` while its bar is dismissed.
    ///
    /// The one gate both the reserved height and the painted strip read, so a
    /// dismissal hands the rows back to the PTY grid instead of leaving a blank
    /// band where the strip used to be.
    fn visible_prompts(&self, session_id: SessionId) -> Option<&PromptBarData> {
        self.prompts.get(&session_id).filter(|data| !data.dismissed)
    }

    /// Hide `session_id`'s prompt bar for the rest of this conversation.
    ///
    /// The flag rides on the prompt record, so it lifts exactly where the
    /// record does: [`Self::note_conversation`] retiring the history on a
    /// conversation switch, or [`Self::forget`] dropping it when the provider
    /// or the session exits — the same boundary the legacy client used.
    fn dismiss(&mut self, session_id: SessionId) {
        if let Some(data) = self.prompts.get_mut(&session_id) {
            data.dismissed = true;
        }
    }

    /// Drop every trace of a session, so a closed pane leaves no orphaned
    /// percentage or prompt history behind.
    fn forget(&mut self, session_id: SessionId) {
        self.tracker.remove(session_id);
        self.tracker.clear_context(session_id);
        self.prompts.remove(&session_id);
        self.conversations.remove(&session_id);
        self.binding_cleared.remove(&session_id);
    }

    /// Forget visible AI state and remember that the provider explicitly
    /// exited, so restore demotes this binding rather than treating the gap as
    /// a fresh AI process that has not emitted its first hook yet.
    fn clear(&mut self, session_id: SessionId) {
        self.forget(session_id);
        self.binding_cleared.insert(session_id);
    }
}

/// A parked server workspace tree plus the live session ids of the
/// `SessionList` that carried it; see the `server_topology` field of
/// [`Shared`].
type ServerTopologySlot = Arc<Mutex<Option<(WorkspaceTreeNode, HashSet<SessionId>)>>>;

/// Coordinates one graceful process exit across every hosted terminal view.
///
/// The deferred restart helper uses SIGTERM only when `QuitAll` cannot reach
/// the server. Signal handling sets `requested`; each foreground view then
/// flushes its own restore state and the final view quits the application.
struct ProcessShutdown {
    requested: Arc<AtomicBool>,
    views: AtomicUsize,
}

impl ProcessShutdown {
    fn install() -> Result<Arc<Self>, String> {
        let requested = Arc::new(AtomicBool::new(false));
        signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&requested))
            .map_err(|error| format!("failed to install graceful SIGTERM handler: {error}"))?;
        Ok(Arc::new(Self { requested, views: AtomicUsize::new(0) }))
    }

    #[cfg(test)]
    fn for_test() -> Arc<Self> {
        Arc::new(Self { requested: Arc::new(AtomicBool::new(false)), views: AtomicUsize::new(0) })
    }

    fn register_view(&self) {
        self.views.fetch_add(1, Ordering::AcqRel);
    }

    #[cfg(test)]
    fn request(&self) {
        self.requested.store(true, Ordering::Release);
    }

    fn requested(&self) -> bool {
        self.requested.load(Ordering::Acquire)
    }

    /// Retire one active view and report whether it was the process's last one.
    fn finish_view(&self) -> bool {
        self.views.fetch_sub(1, Ordering::AcqRel) == 1
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
    /// Actionable warning/error shown in the existing window status bar. Empty
    /// means routine healthy state, which leaves the normal path visible.
    status: Arc<Mutex<String>>,
    generation: Arc<AtomicU64>,
    /// The session in the focused pane: what keystrokes reach and what the
    /// status bar, prompt bar, and tab-context suffix describe.
    active_session: Arc<Mutex<Option<SessionId>>>,
    /// The focused pane's live grid, republished by [`TerminalView::publish_pane_sizes`]
    /// whenever the measured layout or the cell metrics move.
    ///
    /// The IPC reader attaches on its own thread and cannot measure anything, so
    /// it used to carry the nominal [`COLUMNS`]x[`ROWS`] startup box for the
    /// whole life of the window. Every attach then drove the PTY to that fixed
    /// grid before the next redraw drove it back, which is a shrink and a regrow
    /// the foreground process sees as two `SIGWINCH`es. An attach is always
    /// "show this session in the focused pane", so the focused pane's own grid
    /// is the geometry it should carry.
    focused_size: Arc<Mutex<TerminalSize>>,
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
    /// One-shot fresh-window bootstrap. The IPC reader owns the decision and
    /// request because the first `SessionList` can arrive before GPUI builds
    /// the view; the view later claims the staged binding for restore state.
    initial_session: Arc<InitialSessionBootstrap>,
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
    /// Server-owned CI snapshots keyed by repository root.
    ci_runs: Arc<Mutex<CiRunBars>>,
    /// Host actions exist only on this machine's owning local connection.
    ci_owner_controls: bool,
    /// Window close / quit / focus-report state. The view raises a close, a
    /// quit, or a focus change here from a real UI event and the reader folds
    /// the server's `WindowClosed` / `QuitRequested` / `WindowList` answer back
    /// into it; the view's lifecycle tick drains the acknowledged exit.
    lifecycle: Arc<Mutex<WindowLifecycle>>,
    /// Process-wide graceful shutdown shared by every terminal window hosted
    /// in this client process.
    process_shutdown: Arc<ProcessShutdown>,
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
    /// Sessions whose cached pane grid [`reattach_visible_sessions`] left
    /// deliberately unannounced, so [`TerminalView::publish_pane_sizes`] must
    /// forget what it last sent and publish the real grid again. The reader
    /// cannot reach `pane_sizes` — it lives on the view — so it parks the ids
    /// here instead. See [`ReattachPane`] for why a Codex pane takes this route.
    deferred_grids: Arc<Mutex<Vec<SessionId>>>,
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
    /// The server's persisted workspace split tree, parked together with the
    /// live session ids of the `SessionList` that carried it. The reader parks
    /// it on the first list of a connection (before it rebuilds the tab
    /// strip, so no frame can see the sessions without the tree); the GPUI
    /// thread's reconcile pass adopts it when this window has no layout of its
    /// own yet, which is what restores splits across a client restart.
    server_topology: ServerTopologySlot,
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
    /// Server-owned Beads snapshots plus this window's hover/pin intent.
    beads_boards: Arc<Mutex<BeadsBoards>>,
    /// Read-only issue panels and their parked detail requests.
    beads_panels: Arc<Mutex<BeadsPanels>>,
}

/// State shared across the IPC reader and GPUI view for a fresh window's first
/// login shell.
struct InitialSessionBootstrap {
    /// Armed only for a window that is meant to bring its own first shell: no
    /// cold snapshot to replay, no window claimed from the server or shared with
    /// another process. The first authoritative list consumes it even when
    /// non-empty, preventing later reconnects from creating a surprise session
    /// after the original sessions disappear.
    armed: AtomicBool,
    /// Launch metadata staged before `CreateSession` is enqueued. The view
    /// claims it once `SessionCreated` adds the new tab, preserving the launch
    /// id used by environment-envelope and cold-restart persistence.
    binding: Mutex<Option<LaunchBinding>>,
}

impl InitialSessionBootstrap {
    fn new(armed: bool) -> Self {
        Self { armed: AtomicBool::new(armed), binding: Mutex::new(None) }
    }

    /// Consume the one-shot decision on the first list for a connection.
    fn claim(&self, first_on_connection: bool, session_count: usize) -> bool {
        first_on_connection && self.armed.swap(false, Ordering::AcqRel) && session_count == 0
    }
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

/// Cursor blink interval, matching the legacy client and xterm/VTE.
const CURSOR_BLINK_INTERVAL: Duration = Duration::from_millis(530);

/// How long a layout or geometry change must settle before it is written to
/// disk. A drag-resize emits a bounds change per frame and a split re-reports
/// the tree several times while sessions arrive; debouncing collapses each burst
/// into one write of the state the window actually came to rest in.
const RESTORE_DEBOUNCE: Duration = Duration::from_millis(500);

/// How often the client re-polls the server's window list. Mirrors the winit
/// client's throttle: the reply only feeds the status bar's remote-control
/// summary, so it is refreshed on a human timescale rather than per frame.
const WINDOW_LIST_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// A visible pinned board's refresh cadence. Hidden boards never poll.
const BEADS_PINNED_POLL_INTERVAL: Duration = Duration::from_mins(1);
const BEADS_HOVER_REFRESH_AGE: Duration = Duration::from_secs(30);
const BEADS_UNAVAILABLE_RETRY_INTERVAL: Duration = Duration::from_secs(30);

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
    /// A Ctrl+click opened a link. The gesture is already complete, and the
    /// state exists only so the matching release is swallowed rather than
    /// reported to a mouse-tracking application that never saw the press.
    Link,
}

fn hover_focus_target(
    enabled: bool,
    pressed_button: Option<MouseButton>,
    hovered: Option<SessionId>,
    focused: Option<SessionId>,
) -> Option<SessionId> {
    if !enabled || pressed_button.is_some() {
        return None;
    }
    hovered.filter(|session_id| Some(*session_id) != focused)
}

/// Everything the *focused* pane alone gets for one frame.
///
/// The four travel together because they share one reason: each is driven by
/// something the window resolves against exactly one pane — the split-scroll
/// pin, the platform's single input-method slot, the shell cursor, and the
/// pointer. Every other pane paints its own untouched grid.
struct FocusedPanePaint {
    /// The snapshot [`TerminalView::sync_split_scroll`] already pinned.
    content: Arc<Content>,
    ime: ImePaint,
    cursor: CursorPaint,
    /// The link under a Ctrl-held pointer, if any.
    link: Option<HoveredLink>,
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
    /// Snapshots still unclaimed in the restore index after this process took
    /// one. Fanned out as `--restore-child` processes only if the server turns
    /// out to have lost its sessions — see
    /// [`TerminalView::replay_cold_restart`]. A server that kept them restores
    /// its own windows through `Welcome`'s `other_windows` instead, and fanning
    /// out both would open every window twice.
    siblings: usize,
}

impl ColdStart {
    /// A window this process opens itself — a deliberate new window, or one the
    /// server named in `other_windows`. Nothing to replay and no siblings to fan
    /// out; only the geometry the named window was last seen at.
    fn for_window(window_id: Option<WindowId>) -> Self {
        Self { snapshot: None, geometry: window_id.and_then(saved_geometry_for), siblings: 0 }
    }

    /// Claim one restore entry and load the claimed window's geometry.
    fn resolve() -> Self {
        let Some((snapshot, remaining)) = RestoreStore::new().claim_first_window() else {
            return Self::for_window(None);
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
        let siblings = if restore_replay::is_restore_child(std::env::args()) {
            tracing::info!("restore child — not fanning out further windows");
            0
        } else {
            remaining
        };
        // Geometry is keyed by the PRE-CRASH window id. A true cold restart
        // reaches a fresh server that has not named this window yet, and by the
        // time `Welcome` does the window is already on screen.
        let geometry = saved_geometry_for(snapshot.window_id);
        Self { snapshot: Some(snapshot), geometry, siblings }
    }
}

/// The persisted geometry for one window id, normalized and range-checked.
fn saved_geometry_for(window_id: WindowId) -> Option<WindowGeometry> {
    WindowRegistry::new()
        .load_saved(window_id)
        .map(|geom| normalize_legacy_geometry(&geom))
        .filter(geometry_size_is_sane)
        // Clamped into the layout the window is about to open on rather than
        // gated against it: a record whose monitor is gone keeps the placement
        // closest to where the user left it, at a size the remaining screen can
        // actually hold, instead of falling back to the default placement.
        .map(|geom| clamp_geometry_to_layout(&geom, &monitor::connected_monitors()))
}

/// The grid font one appearance config yields at a zoom level.
///
/// The step is folded into the font size here rather than into the config, so
/// every later `appearance.font_size` edit rebases the delta instead of
/// discarding it. Shared by the live zoom step, the config reload, and the
/// restored level a window opens at — three callers that must agree on what a
/// level means or a restart would render at a different size than the zoom did.
fn zoomed_font(appearance: &scribe_common::config::AppearanceConfig, zoom: ZoomState) -> GridFont {
    GridFont::from_appearance(&scribe_common::config::AppearanceConfig {
        font_size: zoom.effective_font_size(appearance.font_size),
        ..appearance.clone()
    })
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
    /// Snapshots left for `--restore-child` siblings; see [`ColdStart::siblings`].
    restore_siblings: usize,
    /// The whole record the window was restored from, adopted as the baseline
    /// the first live capture compares against. Without it a window restored
    /// maximized would drop its pre-maximize rect on the first capture: that
    /// rect only survives by being carried forward from the previous reading.
    ///
    /// The position to re-assert, the monitor to verify it against, and the
    /// minimize state to re-apply are all derived from this one record by
    /// [`RestoreRuntime::from_seed`] rather than carried alongside it.
    restore_geometry: Option<WindowGeometry>,
}

/// Classify what a `CreateSession` is launching so a cold restart relaunches
/// the same thing.
///
/// AI shortcuts bind directly from typed launch intent. Every command without
/// that intent remains a custom command, regardless of its argv contents.
fn launch_binding_for(
    command: Option<&Vec<String>>,
    ai_launch: Option<&AiLaunchSpec>,
    cwd: Option<PathBuf>,
) -> LaunchBinding {
    if let Some(ai_launch) = ai_launch {
        return restore_replay::new_ai_binding(
            ai_launch.provider,
            ai_launch.resume_mode,
            cwd,
            ai_launch.conversation_id.clone(),
        );
    }
    let Some(argv) = command else {
        return restore_replay::new_shell_binding(cwd);
    };
    restore_replay::new_custom_binding(argv.clone(), cwd)
}

type RetainedSessionMetadata =
    (Option<(AiProvider, Option<String>)>, Option<PathBuf>, Option<String>, bool);

fn update_binding_cwd(binding: &mut LaunchBinding, cwd: Option<&Path>) -> bool {
    let Some(cwd) = cwd else { return false };
    if binding.fallback_cwd.as_deref() == Some(cwd) {
        return false;
    }
    binding.fallback_cwd = Some(cwd.to_path_buf());
    true
}

/// Reconcile server-retained metadata onto a session's restore binding.
///
/// The launch id is deliberately never replaced: a live session keeps the
/// environment-envelope identity it was created with. AI state edges promote
/// shell fallbacks to structured resume bindings, while partial edges preserve
/// the last conversation id exactly as the retired client did.
fn update_retained_binding(
    binding: &mut LaunchBinding,
    ai: Option<(AiProvider, Option<&str>)>,
    cwd: Option<&Path>,
) -> bool {
    let changed = update_binding_cwd(binding, cwd);
    let Some((provider, retained_conversation_id)) = ai else { return changed };
    let effective_conversation_id = retained_conversation_id.map(str::to_owned).or_else(|| {
        if let LaunchKind::Ai { conversation_id: existing_conversation_id, .. } = &binding.kind {
            existing_conversation_id.clone()
        } else {
            None
        }
    });
    if matches!(
        &binding.kind,
        LaunchKind::Ai {
            provider: existing_provider,
            conversation_id: existing_conversation,
            ..
        } if *existing_provider == provider && *existing_conversation == effective_conversation_id
    ) {
        return changed;
    }
    binding.kind = LaunchKind::Ai {
        provider,
        resume_mode: AiResumeMode::Resume,
        conversation_id: effective_conversation_id,
    };
    true
}

/// Demote a binding only after an explicit provider-exit edge.
fn clear_retained_binding(binding: &mut LaunchBinding, cwd: Option<&Path>) -> bool {
    let changed = update_binding_cwd(binding, cwd);
    if matches!(binding.kind, LaunchKind::Shell) {
        return changed;
    }
    binding.kind = LaunchKind::Shell;
    true
}

/// Build the best restore binding available for a server-retained session.
///
/// A local `SessionInfo` carries the original launch id when its envelope
/// belongs to this window. Legacy payloads and redacted remote listings omit
/// it and mint once; provider, conversation and CWD still keep replay targeted.
fn retained_session_binding(
    ai: Option<(AiProvider, Option<&str>)>,
    cwd: Option<PathBuf>,
    launch_id: Option<String>,
) -> LaunchBinding {
    let kind = if let Some((provider, conversation_id)) = ai {
        LaunchKind::Ai {
            provider,
            resume_mode: AiResumeMode::Resume,
            conversation_id: conversation_id.map(str::to_owned),
        }
    } else {
        LaunchKind::Shell
    };
    LaunchBinding { launch_id: launch_id.unwrap_or_else(new_launch_id), kind, fallback_cwd: cwd }
}

fn retained_ai_ref(
    ai: Option<&(AiProvider, Option<String>)>,
) -> Option<(AiProvider, Option<&str>)> {
    ai.map(|(provider, conversation)| (*provider, conversation.as_deref()))
}

fn reconcile_retained_binding(
    binding: &mut LaunchBinding,
    ai: Option<&(AiProvider, Option<String>)>,
    cwd: Option<&Path>,
    cleared: bool,
) -> bool {
    if cleared {
        clear_retained_binding(binding, cwd)
    } else {
        update_retained_binding(binding, retained_ai_ref(ai), cwd)
    }
}

/// Everything the window needs to persist its state and replay a cold restart.
///
/// The two halves are deliberately kept together: a restore is only useful with
/// the geometry it was captured at, and both are cleared by the same explicit
/// close or quit.
struct RestoreRuntime {
    store: RestoreStore,
    registry: WindowRegistry,
    /// The snapshot [`ColdStart`] claimed, held until `Welcome` refuses it or
    /// the first `SessionList` says whether it is needed.
    pending: Option<WindowRestoreState>,
    /// The window id of the claimed snapshot. The claim leaves the file on
    /// disk as the last good layout; it is retired here only once this window
    /// has durably written a fresh snapshot of its own.
    claimed_window: Option<WindowId>,
    /// Per-session launch bindings — what a snapshot's `LaunchRecord`s are built
    /// from. Reattached sessions recover structured AI metadata from the
    /// server's retained state; only sessions with no AI identity fall back to
    /// a shell binding.
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
    /// Prompt history the replay read back out of the snapshot, keyed by the
    /// pane it was saved for and drained by [`TerminalView::adopt_session`].
    /// It is parked here because a snapshot files prompts under a pane while
    /// [`AiChrome`] files them under a session, and the session does not exist
    /// until the server answers the pane's replayed launch.
    restored_prompts: HashMap<PaneId, RestoredPrompts>,
    /// Snapshots still to be fanned out as `--restore-child` processes, spawned
    /// once — and only if — this window's own replay confirms the server is
    /// cold. Zeroed on the way out so a redial cannot spawn them twice.
    siblings: usize,
    /// The saved position still to be re-applied to the live window, with the
    /// state to re-assert it under, taken by the first paint. `None` once
    /// applied, or when nothing was persisted.
    pending_position: Option<((i32, i32), WindowState)>,
    /// Where that move was aiming, and when it was asked for. Kept until the
    /// request has had time to be answered and the placement has been checked.
    position_target: Option<((i32, i32), Instant)>,
    /// The monitor the saved position was captured on, for that check. `None`
    /// when the record names no monitor, which makes the check unanswerable.
    monitor: Option<String>,
    /// The state the restored window still has to be re-minimized out of,
    /// taken by the first paint. `None` once applied, or when the record was
    /// not captured minimized.
    pending_minimize: Option<WindowState>,
    /// The maximized or fullscreen state the restored window still has to be
    /// asserted into, taken by the first paint. `None` once applied, or for a
    /// record whose state owns no `_NET_WM_STATE` atoms.
    pending_state: Option<WindowState>,
    /// Where that assert is aiming, when the last `_NET_WM_STATE_ADD` went
    /// out, and how many have gone out. A client message sent before the
    /// window manager has managed the window is dropped rather than queued —
    /// EWMH 5.7 addresses mapped windows — and Mutter takes long enough to
    /// map that the first-paint assert can lose that race. Kept until the
    /// live state reads back as the one asserted, re-asserting on the same
    /// [`RESTORE_DEBOUNCE`] the position verify waits, and given up after
    /// [`STATE_ASSERT_ATTEMPTS`] so a window manager that refuses the state
    /// is argued with a bounded number of times rather than looped against.
    state_target: Option<StateAssert>,
    /// The virtual desktop the restored window still has to be sent back to,
    /// taken by the first paint. `None` once applied, or when the record names
    /// no desktop.
    pending_desktop: Option<u32>,
    /// Whether the record of the window the server assigns is still owed a
    /// read; see [`TerminalView::adopt_assigned_geometry`].
    assigned_geometry: AssignedGeometry,
    /// How far the restore's placement has got. Gates geometry persistence so a
    /// restore cannot save over the record it was aiming at.
    placement: RestorePlacement,
}

/// One replayed pane's persisted AI history, held from the replay dispatch
/// until the pane adopts the session its launch was answered with.
struct RestoredPrompts {
    prompts: PromptBarData,
    /// The conversation the prompts were recorded in, taken from the launch
    /// record's `LaunchKind::Ai`. Seeding it is what keeps the resumed
    /// provider's first state edge from reading as a conversation switch.
    conversation_id: Option<String>,
}

/// Whether this window still has a geometry record to read for the window the
/// server assigns it.
///
/// Only a process that opened without one does: it could not name a window in
/// `Hello`, so which window it is holding is not known until `Welcome` answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AssignedGeometry {
    /// Opened at the default — the assigned window's record is still owed a read.
    Unread,
    /// Opened at its own record, or the assigned window's has been read.
    Adopted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestoreClaimDisposition {
    Waiting,
    Warm,
    Cold,
}

/// Decide whether an accepted claim is still waiting, warm, or cold.
fn restore_claim_disposition(
    claimed: Option<WindowId>,
    assigned: Option<WindowId>,
    session_list_seen: bool,
    live: usize,
) -> RestoreClaimDisposition {
    if !session_list_seen || claimed.is_none() || assigned.is_none() || claimed != assigned {
        return RestoreClaimDisposition::Waiting;
    }
    if live > 0 { RestoreClaimDisposition::Warm } else { RestoreClaimDisposition::Cold }
}

/// How many `_NET_WM_STATE_ADD` messages a restore may send before adopting
/// whatever state the window manager left the window in. Four spans about two
/// seconds at [`RESTORE_DEBOUNCE`] pacing, which covers a compositor still
/// managing the map; a window manager that has refused four idempotent adds
/// is enforcing something real.
const STATE_ASSERT_ATTEMPTS: u8 = 4;

/// One in-flight window-state assert: what was asked, when the last add went
/// out, and how many have gone out.
#[derive(Debug, Clone, Copy)]
struct StateAssert {
    state: WindowState,
    asked_at: Instant,
    attempts: u8,
}

/// How far a restored window's placement has got, which is what decides whether
/// the window's current bounds are the user's layout or the restore's.
///
/// A restored window's move is an asynchronous request the window manager
/// answers when it likes. Every reading taken before it has been answered is
/// the placement the restore is trying to undo, and persisting it overwrites
/// the only record the next start has to aim at. That is what turned a single
/// misplaced restore into a permanent one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestorePlacement {
    /// A saved position is still to be applied, or its landing still to be
    /// checked.
    Restoring,
    /// The landing was checked at this instant and gets one more
    /// [`RESTORE_DEBOUNCE`] of grace, so a window manager still nudging the
    /// window (a strut, a snap) cannot have that reading adopted as the user's.
    Verifying(Instant),
    /// The restore is over; bounds changes from here are the user's.
    Settled,
}

impl RestorePlacement {
    /// Advance the placement and report whether the restore has converged.
    ///
    /// `restore_pending` is true while [`RestoreRuntime::pending_position`],
    /// [`RestoreRuntime::position_target`], [`RestoreRuntime::pending_state`],
    /// or [`RestoreRuntime::state_target`] still hold work — a capture taken
    /// while the state assert is in flight is the windowed reading the assert
    /// exists to undo, and persisting it demotes the record to windowed.
    /// [`TerminalView::verify_restored_position`] runs immediately before every
    /// geometry capture, so all being empty means the landing has just been
    /// checked — or that there was never anything to restore, which is why a
    /// window opened without a record starts [`Self::Settled`].
    fn settled(&mut self, restore_pending: bool) -> bool {
        *self = match *self {
            Self::Restoring if restore_pending => Self::Restoring,
            Self::Restoring => Self::Verifying(Instant::now()),
            Self::Verifying(at) if at.elapsed() < RESTORE_DEBOUNCE => Self::Verifying(at),
            _ => Self::Settled,
        };
        *self == Self::Settled
    }
}

impl RestoreRuntime {
    /// Take the restore inputs the window was opened with, deriving the
    /// placement bits from the one geometry record that already holds them.
    fn from_seed(seed: WindowSeed) -> Self {
        let WindowSeed { restored: pending, restore_siblings, restore_geometry, .. } = seed;
        let mut runtime = Self {
            store: RestoreStore::new(),
            registry: WindowRegistry::new(),
            claimed_window: pending.as_ref().map(|snapshot| snapshot.window_id),
            pending,
            siblings: restore_siblings,
            // A window opened without a record has no placement to reach, so it
            // persists the bounds it came up at from the first capture.
            placement: RestorePlacement::Settled,
            pending_position: None,
            position_target: None,
            monitor: None,
            pending_minimize: None,
            pending_state: None,
            state_target: None,
            pending_desktop: None,
            assigned_geometry: if restore_geometry.is_none() {
                AssignedGeometry::Unread
            } else {
                AssignedGeometry::Adopted
            },
            bindings: HashMap::new(),
            requested: VecDeque::new(),
            window_id: None,
            geometry: None,
            saved_geometry: None,
            layout_dirty_since: None,
            geometry_dirty_since: None,
            cleared: false,
            replaying: false,
            restored_prompts: HashMap::new(),
        };
        if let Some(geometry) = restore_geometry {
            runtime.adopt_geometry_record(geometry);
            // A record with only a state to reach keeps the placement open
            // too: persisting the pre-assert reading would demote the record
            // to windowed exactly the way a pre-move reading misplaced one.
            if runtime.pending_position.is_some() || runtime.pending_state.is_some() {
                runtime.placement = RestorePlacement::Restoring;
            }
        }
        runtime
    }

    /// Adopt the id from `Welcome`, dropping a restore claim the server refused.
    fn adopt_assigned_window(&mut self, window_id: WindowId) {
        self.window_id = Some(window_id);
        let Some(claimed) = self.claimed_window else { return };
        if claimed == window_id {
            return;
        }
        tracing::info!(
            %claimed,
            assigned = %window_id,
            "server refused the restore claim; keeping the fresh window empty"
        );
        self.pending = None;
        self.claimed_window = None;
        self.siblings = 0;
        self.mark_layout_dirty();
    }

    /// Take a persisted record as this window's restore baseline, deriving the
    /// placement bits from the one record that already holds them.
    ///
    /// The caller decides what the [`RestorePlacement`] becomes: a window
    /// opened at the record's bounds only has a position left to reach, while
    /// one that adopted a record after the fact is not at those bounds at all.
    fn adopt_geometry_record(&mut self, geometry: WindowGeometry) {
        // A minimized record cannot be opened minimized — GPUI maps the platform
        // window inside `Window::new` — so the window comes up in the state it
        // would unminimize to and is re-minimized from the first frame.
        self.pending_minimize = (geometry.state == WindowState::Minimized)
            .then(|| WindowGeometry::effective_state(&geometry));
        // EWMH 5.5 puts the virtual desktop in the same class: it is honoured
        // for a withdrawn window through a pre-map property, which the in-
        // `Window::new` map likewise puts out of reach, so it is re-asserted
        // from the first frame too.
        self.pending_desktop = geometry.desktop;
        // The state itself is asserted rather than left to the bounds the window
        // was opened with: GPUI reaches its X11 backend as a *toggle*, so a
        // window manager that maps the window already maximized turns "open
        // maximized" into "unmaximize". See [`monitor::assert_window_state`].
        // Minimized is not here — `pending_minimize` owns it, and the state a
        // minimized window unminimizes into is asserted the same way.
        self.pending_state = matches!(
            WindowGeometry::effective_state(&geometry),
            WindowState::Maximized | WindowState::Fullscreen
        )
        .then(|| WindowGeometry::effective_state(&geometry));
        // A maximized or fullscreen window has a placement too: the window
        // manager owns its size, but the monitor it fills follows the origin,
        // and that origin is exactly the hint Mutter ignores. It is re-asserted
        // with the state lifted around the move (see `apply_saved_position`).
        self.pending_position = geometry
            .restore_origin()
            .map(|origin| (origin, WindowGeometry::effective_state(&geometry)));
        // The nil-UUID placeholder legacy records carry is not a connector name
        // and can never match one; verifying against it would warn on every
        // such start.
        self.monitor = self
            .pending_position
            .is_some()
            .then(|| geometry.monitor_name.clone())
            .flatten()
            .filter(|name| name != NIL_MONITOR_ID);
        self.geometry = Some(geometry);
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
    /// Workspace-region divider drag captured from the grid overlay.
    workspace_divider_drag: Option<WorkspaceDividerDrag>,
    /// The button currently forwarded to the application, or `None` when no
    /// forwarded press is outstanding. Mode 1002 gates drag motion on it, and
    /// the reported Cb carries this exact button rather than a hardcoded Left.
    report_button: Option<MouseButton>,
    /// The cell the last motion report named, for xterm's "reported only if the
    /// pointer has moved to a different character cell" de-duplication.
    report_cell: Option<(u16, u16)>,
    /// Which prompt-strip target the pointer is over, and the session whose
    /// pane owns that strip. Keyed by session because a split window paints
    /// one strip per AI pane, and only the pointed-at one may tint or show its
    /// dismiss control.
    prompt_hover: Option<(SessionId, prompt_bar::PromptBarHover)>,
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
            workspace_divider_drag: None,
            report_button: None,
            report_cell: None,
            prompt_hover: None,
        }
    }
}

struct TerminalFocus {
    root: FocusHandle,
    /// Stable tab stop for the actionable update CTA. Keeping the handle on
    /// the view prevents ordinary repaints from returning focus to the PTY.
    update: FocusHandle,
    cursor_blink: CursorBlink,
}

fn focus_is_unclaimed(claims: [bool; 5]) -> bool {
    !claims.into_iter().any(std::convert::identity)
}

impl TerminalFocus {
    fn new(window_active: bool, cx: &mut Context<TerminalView>) -> Self {
        Self {
            root: cx.focus_handle(),
            update: cx.focus_handle().tab_stop(true),
            cursor_blink: CursorBlink::new(window_active),
        }
    }
}

/// Window-level shell-cursor focus and blink phase.
struct CursorBlink {
    visible: bool,
    window_active: bool,
    last_toggle: Instant,
}

impl CursorBlink {
    fn new(window_active: bool) -> Self {
        Self { visible: true, window_active, last_toggle: Instant::now() }
    }

    /// Start a fresh visible phase after focus or keyboard activity.
    fn show_now(&mut self) -> bool {
        let changed = !self.visible;
        self.visible = true;
        self.last_toggle = Instant::now();
        changed
    }

    fn set_window_active(&mut self, active: bool) {
        self.window_active = active;
        self.show_now();
    }
}

type VisibleCiRun = (WorkspaceId, PathBuf, CiRunState, Option<CiRunDetails>);

struct TerminalView {
    shared: Shared,
    sink: IpcSink,
    focus: TerminalFocus,
    /// Live config: the resolved snapshot plus the file watcher that keeps it
    /// fresh. Held for the window's lifetime — dropping it stops the watcher.
    config: ConfigRuntime,
    /// System-stats sampler feeding the status bar's CPU/MEM/NET/GPU sparklines.
    stats: SystemStatsCollector,
    /// Theme-derived status-bar palette, rebuilt on every theme reload.
    status_colors: StatusBarColors,
    /// The Beads board's palette, derived from the theme like every other
    /// per-surface palette here.
    beads_colors: BeadsBoardColors,
    /// Window-exclusive native input owner for one armed Beads passage.
    beads_editor: Entity<BeadsEditor>,
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
    /// Where each pane painted its grid last frame, filled in by that pane's
    /// own grid canvas so a pointer position can be lowered onto a cell.
    ///
    /// Keyed by session because the pointer gestures that need a rect do not
    /// all belong to the focused pane: the wheel and the overlay scrollbar act
    /// on whichever pane is under the pointer, and a split window paints their
    /// rects side by side. One sink per pane is the only thing that keeps a hit
    /// test agreeing with what paint actually drew.
    pane_bounds: HashMap<SessionId, GridBounds>,
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
    /// The `terminal.prompt_bar` config: enabled flag and strip position.
    prompt_bar: TerminalPromptBarConfig,
    /// The window's live pane and workspace split layout. Every pane action
    /// mutates it and the render pass resolves it into the grid area's panes.
    shell: PaneShell,
    /// The grid geometry last published to the server for each pane's session,
    /// so a redraw only re-sends `Resize` when a split actually changed a
    /// pane's size.
    pane_sizes: HashMap<SessionId, TerminalSize>,
    /// One projected-GPU-bounded source cache shared by every pane in this view.
    image_cache: Rc<RefCell<GpuiImageCache>>,
    /// Geometry of the focused pane, which is where a new tab or a split's
    /// session opens. Starts at the whole window and shrinks with the layout.
    focused_pane_size: TerminalSize,
    /// The split trees this view recently put on the wire, newest last, so a
    /// repaint that left the topology alone does not re-report it. Bounded at
    /// [`Self::REPORTED_TREE_HISTORY`]; empty until the first report. Kept as
    /// a history rather than one value so a reconnect can tell "the server's
    /// stored tree is one of ours" (a mid-session redial, possibly a few
    /// queued reports behind) from "another client reshaped this window since
    /// we last reported" — the stale-claim case that must adopt the server's
    /// tree instead of imposing this view's old layout back onto it.
    reported_trees: VecDeque<scribe_common::protocol::WorkspaceTreeNode>,
    // The custom titlebar + integrated tab bar drawn above the terminal grid.
    titlebar: Entity<TitlebarView>,
    /// Theme chrome, retained to build the overlay palettes on demand.
    chrome: ChromeColors,
    /// Terminal dimensions announced to the server for newly created sessions.
    terminal_size: TerminalSize,
    /// Last tab strip pushed into the titlebar, so a redraw only re-renders the
    /// tab row when the shared model actually changed.
    rendered_tabs: TabSessions,
    /// For each titlebar tab position, the strip index it renders. The titlebar
    /// hosts only top-row regions' tabs — lower regions carry their own bars —
    /// so its Select/Close/Reorder events arrive in titlebar positions and are
    /// translated through this map before touching [`TabSessions`].
    titlebar_slots: Vec<TabAddress>,
    /// Tab-bar contents for each workspace region below the window's top row,
    /// rebuilt by [`Self::sync_tabs`] and painted into the grid band at each
    /// region's top strip.
    region_chrome: RegionChrome,
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
    /// Workspace board painted this frame and whether it is pinned.
    visible_beads_boards: Vec<(WorkspaceId, bool)>,
    /// CI snapshots matched to the regions that currently own their repository.
    visible_ci_runs: Vec<VisibleCiRun>,
    /// Client-local open panel identity; the server sees only interest changes.
    ci_expanded: HashMap<WorkspaceId, (PathBuf, String)>,
    /// Stable tab stops for each region's toggle plus owner-only actions.
    ci_action_focus: HashMap<WorkspaceId, (FocusHandle, FocusHandle, FocusHandle)>,
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
    /// Last request sent while a pinned Beads board was visible and focused.
    last_beads_poll: Instant,
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
    /// This view has retired from the process-wide active-view count.
    process_shutdown_finished: bool,
    /// Held to keep the window-bounds observer alive, which is what notices a
    /// move or resize worth persisting.
    _bounds_observer: Subscription,
}

impl Focusable for TerminalView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.root.clone()
    }
}

/// One lower workspace region's tab-bar contents.
///
/// The titlebar hosts tabs only for regions on the window's top row; a region
/// stacked below carries this bar at the top of its own rect instead — the
/// legacy client's per-workspace tab bar, kept out of [`TitlebarView`] because
/// it needs no drag-reorder or window-move chrome.
// ponytail: region-bar tabs skip drag-reorder and AccessKit tab stops; route
// through TitlebarView if either ever matters here.
/// Marker registering GPUI's native drag for a lower region bar's tab.
///
/// Distinct from the titlebar's own marker so a region drag never wakes
/// [`TitlebarView`]'s `on_drag_move`, and so its presence keeps mouse moves
/// flowing to the region bar after the cursor leaves the band.
struct RegionTabDrag;

/// Invisible cursor-following overlay for a region tab drag. The tab itself
/// slides inside the bar, so the overlay paints nothing.
struct RegionTabDragGhost;

impl Render for RegionTabDragGhost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

/// Which bar a region tab belongs to and where it sits in it.
#[derive(Clone, Copy)]
struct RegionTabSlot {
    workspace_id: WorkspaceId,
    index: usize,
}

/// An in-flight drag of a tab inside a lower region's own bar.
///
/// The window titlebar keeps its drag inside `TitlebarView`, but a region bar is
/// rendered inline by the view, so this is where its drag lives. The target slot
/// comes from pointer travel rather than the titlebar's absolute edge walk: a
/// region bar's tab run starts after a workspace pill whose width is derived
/// from the badge text, which a listener has no way to measure. Travel divided
/// by tab width and rounded puts the boundary at half a tab of overlap, which is
/// the same rule the titlebar's centre test applies.
#[derive(Clone, Copy)]
struct RegionTabDragState {
    /// The bar being dragged within. A drag never crosses regions.
    workspace_id: WorkspaceId,
    /// Slot the drag began at, held fixed so travel stays absolute.
    origin: usize,
    /// Slot the tab currently occupies, which is what the next swap moves from.
    current: usize,
    press_x: f32,
    /// Whether any swap committed, so the release can tell a drag from a click.
    reordered: bool,
}

/// The lower regions' tab bars and the drag running inside one of them.
///
/// Grouped because the drag is only ever meaningful against the bars it indexes
/// into: a rebuild that drops a bar has to be able to invalidate its drag.
#[derive(Default)]
struct RegionChrome {
    bars: Vec<RegionBarData>,
    drag: Option<RegionTabDragState>,
}

struct RegionBarData {
    /// The region this bar belongs to, keyed by its (server) workspace.
    workspace_id: WorkspaceId,
    /// Region accent, tinting the bar's hairline and active underline.
    accent: [f32; 4],
    /// Badge pill, present only when the server named the workspace.
    badge: Option<GroupBadge>,
    /// The bar's tabs in strip order, each naming the session it selects.
    tabs: Vec<(SessionId, TabData)>,
}

impl TerminalView {
    /// How many recently reported split trees are kept for the reconnect
    /// authorship check in [`Self::adopt_server_topology`]. Deep enough to
    /// cover a burst of layout edits queued during an outage; a server tree
    /// older than this window is treated as another client's.
    const REPORTED_TREE_HISTORY: usize = 8;

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
        cx.observe_window_bounds(window, |view, window, ctx| {
            // A late bounds change is also a chance to settle the restore move,
            // for a window that is not repainting for any other reason.
            view.verify_restored_position(window);
            view.capture_geometry(window, ctx);
        })
    }

    /// Put a restored window back on the monitor it was on, once.
    ///
    /// The bounds handed to `open_window` are a hint the window manager is free
    /// to ignore, and Mutter does: GPUI sets no `USPosition`/`PPosition` size
    /// hint, so every restored window was placed by the compositor on whatever
    /// screen was active and the saved position was silently discarded. The
    /// move is issued from the first frame — the window has to exist before it
    /// can be moved — and never repeated, so it can neither fight the user
    /// dragging the window nor loop against a window manager that adjusts it.
    ///
    /// A maximized or fullscreen window is moved the same way: GPUI maximizes
    /// it *after* the map (`Window::new` maps, then `zoom()`s), so the window
    /// manager has already chosen the monitor by the time anything can be
    /// asked of it. [`monitor::apply_saved_position`] lifts the state for the
    /// move and puts it back, which re-maximizes the window on the monitor the
    /// origin names.
    ///
    /// Runs before [`Self::capture_geometry`] so the same frame records where
    /// the window actually landed rather than where it started.
    fn apply_saved_position(&mut self, window: &Window) {
        let Some(((x, y), state)) = self.restore.pending_position.take() else { return };
        if monitor::apply_saved_position(window, x, y, state) {
            self.restore.position_target = Some(((x, y), Instant::now()));
            tracing::info!(x, y, ?state, "moving the restored window back to its saved position");
        } else {
            tracing::debug!(x, y, "no X11 window to reposition; keeping WM placement");
        }
    }

    /// Put a restored window back into the maximized or fullscreen state its
    /// record names, once.
    ///
    /// Issued after [`Self::apply_saved_position`], which lifts that same state
    /// around the move and puts it back: this is the backstop for every case
    /// that move does not cover — a record with no saved origin to move to, a
    /// window manager that refused the move, and above all the fact that GPUI's
    /// own "open maximized" is a toggle rather than an assertion, so a window
    /// mapped already-maximized arrives here windowed. `_NET_WM_STATE_ADD` is
    /// idempotent, so doing it after a move that already restored the state
    /// costs one ignored client message.
    fn apply_saved_window_state(&mut self, window: &Window) {
        if let Some(state) = self.restore.pending_state.take() {
            if monitor::assert_window_state(window, state) {
                tracing::info!(?state, "asserting the restored window's saved state");
                self.restore.state_target =
                    Some(StateAssert { state, asked_at: Instant::now(), attempts: 1 });
            } else {
                tracing::debug!(?state, "no X11 window state to assert; keeping what the WM chose");
            }
            return;
        }
        self.verify_restored_state(window);
    }

    /// Check that the asserted state actually took, re-asserting until it does.
    ///
    /// The first assert goes out around the first paint, which on X11 can be
    /// before the window manager has managed the window at all — GPUI's map is
    /// a request the WM answers when it likes, and Mutter answers it through a
    /// placement pass and a map animation. EWMH 5.7's `_NET_WM_STATE` client
    /// message addresses mapped windows, so one sent early is not queued but
    /// dropped, and the window relaunches windowed while its record says
    /// maximized. Unlike the position — where the WM's different answer is
    /// respected — the add is re-sent: it is idempotent, and the only party
    /// that asked for windowed here is the race. [`STATE_ASSERT_ATTEMPTS`]
    /// bounds the argument, and the give-up is logged so a refusing window
    /// manager is visible rather than silent.
    fn verify_restored_state(&mut self, window: &Window) {
        let Some(StateAssert { state, asked_at, attempts }) = self.restore.state_target else {
            return;
        };
        if asked_at.elapsed() < RESTORE_DEBOUNCE {
            return;
        }
        // The same EWMH reading capture trusts; GPUI's own answer is only the
        // off-X11 fallback, and an armed target means X11 answered the assert.
        let fallback = window
            .is_fullscreen()
            .then_some(WindowState::Fullscreen)
            .or_else(|| window.is_maximized().then_some(WindowState::Maximized))
            .unwrap_or_default();
        let observed = monitor::observed_window_state(
            window,
            ObservedWindowState { state: fallback, ..ObservedWindowState::default() },
        )
        .state;
        if observed == state {
            tracing::info!(?state, attempts, "the restored window's saved state landed");
            self.restore.state_target = None;
            return;
        }
        if attempts >= STATE_ASSERT_ATTEMPTS {
            tracing::warn!(
                ?state,
                ?observed,
                attempts,
                "the window manager kept the restored window out of its saved state; giving up"
            );
            self.restore.state_target = None;
            return;
        }
        monitor::assert_window_state(window, state);
        self.restore.state_target =
            Some(StateAssert { state, asked_at: Instant::now(), attempts: attempts + 1 });
    }

    /// Put a restored window back on its saved virtual desktop, once.
    ///
    /// EWMH 5.5 says the window manager "should honor `_NET_WM_DESKTOP`
    /// whenever a withdrawn window requests to be mapped" — a property written
    /// before the map, which GPUI's map inside `Window::new` puts out of reach
    /// exactly as it does ICCCM's "map me iconified". The post-map form is the
    /// `_NET_WM_DESKTOP` client message to the root, so the window appears on
    /// the current desktop for a frame and is then sent away, the same visible
    /// flash [`Self::apply_pending_minimize`] leaves behind.
    ///
    /// Issued after [`Self::apply_saved_position`]: a window sent to another
    /// desktop is unmapped and stops painting, so the placement check the
    /// restore is waiting on would never run if the move had not gone out
    /// first. It still only completes once the user comes back to that
    /// desktop, which is also the first moment the geometry is worth
    /// re-persisting.
    fn apply_saved_desktop(&mut self, window: &Window) {
        let Some(desktop) = self.restore.pending_desktop.take() else { return };
        if monitor::apply_saved_desktop(window, desktop) {
            tracing::info!(desktop, "sending the restored window back to its saved desktop");
        } else {
            tracing::debug!(desktop, "no EWMH desktop support; keeping the current desktop");
        }
    }

    /// Put a restored window back into its minimized state, once.
    ///
    /// ICCCM 4.1.2.4's "map me iconified" is `WM_HINTS.initial_state`, set
    /// *before* the window is mapped — and GPUI maps inside `Window::new`, so
    /// the only reachable request is the post-map one: `minimize_window`, which
    /// on X11 is the ICCCM `WM_CHANGE_STATE` client message. The window
    /// therefore flashes visible for a frame on the way down; removing that
    /// needs the pre-map hint, which only GPUI can set.
    ///
    /// Issued after [`Self::apply_saved_position`] so the window manager has
    /// somewhere to put the window when the user brings it back. A hidden
    /// window stops painting, so the placement check the restore is waiting on
    /// only runs once the user unminimizes it — which is also the first moment
    /// the geometry is worth re-persisting.
    fn apply_pending_minimize(&mut self, window: &Window) {
        let Some(restore_state) = self.restore.pending_minimize.take() else { return };
        tracing::info!(?restore_state, "re-minimizing the restored window");
        window.minimize_window();
    }

    /// Check where the restored window actually landed, once.
    ///
    /// The question that matters is not the pixel residual — `StaticGravity`
    /// removes the frame ambiguity that used to produce one — but whether the
    /// window is on the monitor its record names. A window manager is free to
    /// answer the move with a strut, a snap, an off-screen clamp, or the active
    /// monitor, and silently accepting the wrong screen is what made a bad
    /// restore look like a good one.
    ///
    /// The reading is taken only once the request has had [`RESTORE_DEBOUNCE`]
    /// to be answered: the move is asynchronous, so reading back beside it — or
    /// on the next bounds change, which can still be an earlier one arriving —
    /// reports a placement the window has not reached yet. Nothing is
    /// re-asserted; a window manager that has put the window elsewhere is
    /// enforcing something real, and arguing with it is how a placement loop
    /// starts. It is logged instead, so the give-up is explicit.
    fn verify_restored_position(&mut self, window: &Window) {
        let (want_x, want_y) = match self.restore.position_target {
            Some((target, asked_at)) if asked_at.elapsed() >= RESTORE_DEBOUNCE => target,
            _ => return,
        };
        self.restore.position_target = None;
        let origin = window.bounds().origin;
        // The same rounding the record was written with, so the comparison is
        // between two values in one space rather than two roundings of one.
        let (landed_x, landed_y) =
            (logical_px_to_i32(f32::from(origin.x)), logical_px_to_i32(f32::from(origin.y)));
        // No saved monitor, or no RandR to resolve the live one (Wayland,
        // macOS, headless): the position is all there is to report.
        let (Some(want_monitor), Some(landed_monitor)) =
            (self.restore.monitor.as_deref(), monitor::window_monitor_name(window))
        else {
            tracing::info!(want_x, want_y, landed_x, landed_y, "restored the window's position");
            return;
        };
        if landed_monitor == want_monitor {
            tracing::info!(
                want_x,
                want_y,
                landed_x,
                landed_y,
                monitor = want_monitor,
                "restored the window to its saved monitor"
            );
            return;
        }
        tracing::warn!(
            want_x,
            want_y,
            landed_x,
            landed_y,
            want_monitor,
            landed_monitor,
            "the window manager placed the restored window on a different monitor; giving up"
        );
    }

    /// Capture the live window's geometry into the restore runtime.
    ///
    /// Runs once at construction and again on every bounds change, which is the
    /// GPUI equivalent of the winit client's `Moved`/`Resized` handlers. The
    /// write itself is debounced by the lifecycle tick — a drag-resize would
    /// otherwise rewrite the file once per frame.
    ///
    /// Nothing is persisted until the restore's own moves have converged (see
    /// [`RestorePlacement`]). Until then the reading is tracked but adopted as
    /// the baseline, so a restore that landed on the wrong monitor leaves the
    /// record it was aiming at intact for the next start to try again from.
    fn capture_geometry(&mut self, window: &Window, cx: &App) {
        let settled = self.restore.placement.settled(
            self.restore.pending_position.is_some()
                || self.restore.position_target.is_some()
                || self.restore.pending_state.is_some()
                || self.restore.state_target.is_some(),
        );
        // RandR connector name where available; GPUI's X11 display uuid is a
        // nil placeholder and must never be persisted (see `monitor`).
        let monitor = monitor::persisted_monitor_name(window, cx);
        // GPUI answers maximized and fullscreen but has no minimized query, and
        // its X11 `is_maximized()` goes false the moment the window is hidden;
        // the window manager's own `_NET_WM_STATE` is the only reading that
        // survives minimization. GPUI's answer is the off-X11 fallback.
        let fallback_state = window
            .is_fullscreen()
            .then_some(WindowState::Fullscreen)
            .or_else(|| window.is_maximized().then_some(WindowState::Maximized))
            .unwrap_or_default();
        // GPUI cannot report minimized, so the fallback is never hidden and
        // never has a restore state of its own.
        let observed = monitor::observed_window_state(
            window,
            ObservedWindowState { state: fallback_state, ..ObservedWindowState::default() },
        );
        // Wayland answers (0, 0) for every window, and persisting that put a
        // later X11 start in the screen corner; the record stores no origin at
        // all instead, and restore falls back to the default placement.
        let bounds = window.bounds();
        let geometry = geometry_from_bounds(
            monitor::window_origin_is_exposed(window).then_some(bounds.origin),
            bounds.size,
            observed,
            monitor,
            self.restore.geometry.as_ref(),
        )
        // EWMH `_NET_WM_DESKTOP` is WM-owned the same way `_NET_WM_STATE` is; a
        // window manager without virtual desktops publishes none and the record
        // simply carries no desktop to restore.
        .on_desktop(monitor::window_desktop(window))
        // The zoom level is captured here rather than written from the zoom
        // step itself: this runs on every frame, so a step's `cx.notify()`
        // brings the new level through the same equality check that arms the
        // debounce for a move or a resize, and there is no second write path.
        .at_zoom(self.zoom.level())
        // Pinned boards ride the same capture as the zoom level: a pin is
        // per-window state the user chose, and the record is the only place it
        // survives a quit.
        .with_pinned_boards(self.pinned_board_ids());
        if !geometry_size_is_sane(&geometry) || self.restore.geometry.as_ref() == Some(&geometry) {
            return;
        }
        self.restore.geometry = Some(geometry.clone());
        if !settled {
            // Where the restore itself left the window is not a layout the user
            // chose. Recording it as already-persisted keeps both the debounced
            // flush and the quit-time flush off the saved record, while the
            // user's next move still differs from it and arms normally.
            self.restore.saved_geometry = Some(geometry);
            return;
        }
        if self.restore.geometry_dirty_since.is_none() {
            self.restore.geometry_dirty_since = Some(Instant::now());
        }
    }

    /// Workspaces whose boards are pinned open, in the order the record keeps.
    fn pinned_board_ids(&self) -> Vec<WorkspaceId> {
        self.shared.beads_boards.lock().map(|boards| boards.pinned()).unwrap_or_default()
    }

    /// The zoom level a window opens at, and the grid font that level yields.
    ///
    /// A restored window comes back at the level it was left at: zoom is
    /// per-window state the user set deliberately, and the geometry record is
    /// the only place it survived the quit. The font is built at the effective
    /// size here rather than rebuilt a frame later, so the window never paints
    /// a frame at the configured size on its way to the restored one.
    fn opening_font(config: &ConfigRuntime, seed: &WindowSeed) -> (ZoomState, GridFont) {
        let zoom = ZoomState::at_level(seed.restore_geometry.as_ref().map_or(0, |geom| geom.zoom));
        (zoom, zoomed_font(&config.config().config.appearance, zoom))
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

    #[allow(clippy::too_many_lines, reason = "view construction lists its owned surfaces once")]
    fn new(
        shared: Shared,
        sink: IpcSink,
        seed: WindowSeed,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        shared.process_shutdown.register_view();
        let (bounds_observer, (x11_focus, x11_focus_task)) =
            (Self::start_geometry_tracking(window, cx), Self::start_x11_focus_guard(window, cx));
        let activation_observer = cx
            .observe_window_activation(window, |view, window, ctx| view.on_activation(window, ctx));
        let (bell, bell_subscription) = Self::start_bell_gate(window, cx);
        Self::register_close_veto(window, cx);
        // Start config watching before constructing surfaces that consume it.
        let config = ConfigRuntime::start();
        let notifications = Self::start_notifications(&shared, &config, window, cx);
        let drivers = Self::start_drivers(Arc::clone(&shared.generation), config.signal(), cx);
        let (status_colors, terminal_colors, scrollbar_style) = Self::theme_palettes(&config);
        let (chrome, opacity) = (config.config().chrome, clamp_opacity(config.opacity()));
        let beads_colors = BeadsBoardColors::from_theme(
            &config.config().theme.chrome,
            &config.config().theme.ansi_colors,
            opacity,
        );
        let beads_editor =
            cx.new(|ctx| BeadsEditor::new(Arc::clone(&shared.beads_panels), window, ctx));
        let (zoom, font) = Self::opening_font(&config, &seed);
        let stats_config = config.config().config.terminal.status_bar_stats.clone();
        let terminal = &config.config().config.terminal;
        let smart_selection = compile_smart_selection(&terminal.smart_selection);
        let context_thresholds = terminal.ai_session.context_thresholds.clone();
        let prompt_bar = terminal.prompt_bar.clone();
        let gate = Self::start_paste_gate(terminal.paste_confirmation, cx);
        let titlebar = Self::build_titlebar(&chrome, opacity, cx);
        Self {
            shared,
            sink,
            focus: TerminalFocus::new(window.is_window_active(), cx),
            config,
            stats: SystemStatsCollector::new(),
            status_colors,
            beads_colors,
            beads_editor,
            stats_config,
            font,
            zoom,
            smart_selection,
            split_scroll: SplitScrollState::new(),
            pane_bounds: HashMap::new(),
            scrollbars: ScrollbarSurfaces::new(scrollbar_style),
            grid_area: GridBounds::default(),
            published_grid_area: None,
            prompt_colors: PromptBarColors::from(&chrome),
            context_thresholds,
            prompt_bar,
            shell: PaneShell::new(chrome.accent, cx),
            pane_sizes: HashMap::new(),
            image_cache: Rc::new(RefCell::new(GpuiImageCache::new())),
            focused_pane_size: seed.terminal_size,
            reported_trees: VecDeque::new(),
            titlebar,
            chrome,
            terminal_size: seed.terminal_size,
            // The strip starts empty and is filled by the reader's first
            // `SessionList`; `sync_tabs` pushes it into the titlebar on the
            // next redraw so the tab row always mirrors live server state.
            rendered_tabs: TabSessions::new(),
            titlebar_slots: Vec::new(),
            region_chrome: RegionChrome::default(),
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
            pending_osc8_uri: None,
            pending_lan_approval: None,
            clipboard: ClipboardSurfaces::new(gate),
            pointer: PointerState::default(),
            tooltip_demo: false,
            visible_beads_boards: Vec::new(),
            visible_ci_runs: Vec::new(),
            ci_expanded: HashMap::new(),
            ci_action_focus: HashMap::new(),
            x11_focus,
            ime: Self::start_ime(cx),
            bell,
            notifications,
            last_window_list_poll: Instant::now(),
            last_beads_poll: Instant::now(),
            _refresh_task: drivers.1,
            _config_task: drivers.2,
            _x11_focus_task: x11_focus_task,
            _lifecycle_task: drivers.0,
            _activation_observer: activation_observer,
            _bell_subscription: bell_subscription,
            restore: RestoreRuntime::from_seed(seed),
            process_shutdown_finished: false,
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
        // Read off the published projection rather than the grid: this runs on
        // every paint, and a paint must never queue behind a VTE parse.
        let placement = self
            .focused_session()
            .and_then(|session_id| self.pane_frame(session_id))
            .map(|frame| frame.cursor);
        let preedit = self.ime.update(cx, |ime, _| {
            if let Some(placement) = placement {
                ime.set_anchor(placement.abs_row, placement.col);
            }
            ime.preedit().cloned()
        });
        ImePaint {
            focus_handle: self.focus.root.clone(),
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
        // The titlebar is a flat row of pixels holding several regions' tabs, so
        // every event position is resolved to the tab it actually rendered
        // before it can mean anything.
        cx.subscribe(&bar, |this, _bar, event: &TitlebarEvent, ctx| match event {
            TitlebarEvent::ReorderTab { from, to } => {
                let (Some(from), Some(to)) = (this.titlebar_slot(*from), this.titlebar_slot(*to))
                else {
                    return;
                };
                // A drag that left its own region is dropped rather than
                // clamped: the two ends address different tab lists, so there
                // is no position in `from`'s region that `to` names.
                if from.workspace_id != to.workspace_id {
                    return;
                }
                let moved =
                    this.shared.tabs.lock().is_ok_and(|mut tabs| {
                        tabs.reorder(from.workspace_id, from.index, to.index)
                    });
                if moved {
                    // The strip is the region's tab order, and the reported
                    // tree is the only place it is durable — without this the
                    // drag survives until the next reconnect and no longer.
                    this.report_workspace_tree(ctx);
                    ctx.notify();
                }
            }
            TitlebarEvent::SelectTab { index, source } => {
                let Some(slot) = this.titlebar_slot(*index) else { return };
                let source = *source;
                this.activate_session_tab(slot.session_id, ctx);
                if source == TabActivationSource::Pointer {
                    this.defer_terminal_focus(ctx);
                }
            }
            TitlebarEvent::CloseTab(index) => {
                if let Some(slot) = this.titlebar_slot(*index) {
                    this.close_session(slot.session_id, "tab close dropped");
                }
            }
            TitlebarEvent::Equalize => this.equalize_layout(ctx),
            TitlebarEvent::BeadsHover { index, hovered } => {
                let Some(workspace_id) = this.titlebar_slot(*index).map(|slot| slot.workspace_id)
                else {
                    return;
                };
                let refresh = this.shared.beads_boards.lock().is_ok_and(|mut boards| {
                    boards.hover(workspace_id, HoverSource::Bead, *hovered);
                    boards.needs_refresh(workspace_id, BEADS_HOVER_REFRESH_AGE)
                });
                if *hovered && refresh {
                    request_beads_board_or_log(&this.sink, workspace_id, "hover refresh");
                }
                ctx.notify();
            }
            TitlebarEvent::ToggleBeadsBoard { index } => {
                let Some(workspace_id) = this.titlebar_slot(*index).map(|slot| slot.workspace_id)
                else {
                    return;
                };
                let refresh = this.shared.beads_boards.lock().is_ok_and(|mut boards| {
                    boards.toggle_pin(workspace_id);
                    boards.needs_refresh(workspace_id, BEADS_HOVER_REFRESH_AGE)
                });
                if refresh {
                    request_beads_board_or_log(&this.sink, workspace_id, "pin refresh");
                }
                ctx.notify();
            }
        })
        .detach();
        bar
    }

    /// Resolve a titlebar tab position to the tab it renders.
    ///
    /// `None` before the first [`Self::sync_tabs`] fills the slots, and for a
    /// stale position past their end — a racing event then does nothing rather
    /// than acting on whichever tab now happens to sit at that position.
    fn titlebar_slot(&self, titlebar_index: usize) -> Option<TabAddress> {
        self.titlebar_slots.get(titlebar_index).copied()
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
            self.rebuild_beads_colors();
            self.push_tab_bar_colors(cx);
            // Open overlays captured the old palette when they were built; drop
            // them so a live theme edit never leaves stale colours on screen.
            self.command_palette = None;
            self.close_find_overlay();
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
        self.prompt_bar = terminal.prompt_bar.clone();
        self.smart_selection = compile_smart_selection(&terminal.smart_selection);
        if let Ok(mut ai) = self.shared.ai.lock() {
            ai.tracker.reconfigure(terminal.ai_session.ai_states.clone());
        }
        let paste_confirmation = terminal.paste_confirmation;
        self.clipboard.gate.update(cx, |gate, _| gate.set_confirmation_enabled(paste_confirmation));
        // The notification gate reads `enabled`, `condition`, and the two
        // timeout fields on every decision, so an edit to `[notifications]` has
        // to reach it or the window keeps firing on the old policy.
        let notifications = self.config.config().config.notifications.clone();
        self.notifications.center.update(cx, |center, _| center.reconfigure(notifications));
        // A cursor setting may have changed even though it does not affect the
        // theme/font/opacity reload plan. Start the new setting in a visible
        // phase so a live edit cannot strand the cursor hidden.
        self.focus.cursor_blink.show_now();

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
        self.rebuild_beads_colors();
        tracing::info!(opacity = self.opacity, "config reload: opacity applied");
    }

    /// Rebuild the board's palette from the live theme and opacity.
    ///
    /// Both inputs move on their own — a theme edit and an opacity edit are
    /// separate reload plans — so this runs from each rather than from one.
    fn rebuild_beads_colors(&mut self) {
        let config = self.config.config();
        self.beads_colors = BeadsBoardColors::from_theme(
            &config.theme.chrome,
            &config.theme.ansi_colors,
            self.opacity,
        );
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

    /// The configured badge colour for a real server-provided workspace name.
    // @lat: [[lat.md/common#Common#Configuration#Workspaces]]
    fn workspace_badge_accent(name: &str, palette: &[String], fallback: [f32; 4]) -> gpui::Rgba {
        if palette.is_empty() {
            return opaque_slot(fallback);
        }
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        std::hash::Hash::hash(name, &mut hash);
        let palette_len = u64::try_from(palette.len()).unwrap_or(u64::MAX);
        let index = usize::try_from(std::hash::Hasher::finish(&hash) % palette_len).unwrap_or(0);
        palette
            .get(index)
            .and_then(|color| scribe_common::theme::hex_to_rgba(color).ok())
            .map_or_else(|| opaque_slot(fallback), opaque_slot)
    }

    fn workspace_group_badge(
        workspace_name: Option<&str>,
        palette: &[String],
        fallback: [f32; 4],
        beads: bool,
    ) -> Option<GroupBadge> {
        let label = badge_label(workspace_name, true)?;
        let accent = Self::workspace_badge_accent(label, palette, fallback);
        Some(GroupBadge { label: label.to_owned(), accent, beads })
    }

    fn workspace_name(&self, workspace_id: WorkspaceId) -> Option<String> {
        self.shared
            .chrome_metadata
            .lock()
            .ok()
            .and_then(|store| store.workspace_name(workspace_id).map(ToOwned::to_owned))
    }

    /// Mark each multi-workspace titlebar run with its region edge and accent,
    /// plus a badge only when the server provides a real workspace name.
    ///
    /// `slots` names, for each titlebar position, the tab it renders — the
    /// titlebar hosts only top-row regions' tabs, whose left edges are distinct
    /// by construction, so the aligned layout always engages in a multi-region
    /// window.
    fn apply_group_badges(&self, data: &mut [TabData], slots: &[TabAddress], cx: &App) {
        let single_region = self.shell.region_count(cx) <= 1;
        let region_x = self.shell.region_left_edges(self.pane_viewport(), cx);
        let mut previous = None;
        for (tab, slot) in data.iter_mut().zip(slots) {
            let accent = self.shell.workspace_accent(slot.workspace_id, cx);
            tab.group_accent = Some(opaque_slot(accent));
            if previous != Some(slot.workspace_id) {
                tab.group_region_x = (!single_region)
                    .then(|| region_x.get(&slot.workspace_id).copied().unwrap_or(0.0));
                let workspace_name = self.workspace_name(slot.workspace_id);
                tab.badge = Self::workspace_group_badge(
                    workspace_name.as_deref(),
                    &self.config.config().config.workspaces.badge_colors,
                    accent,
                    self.shared
                        .beads_boards
                        .lock()
                        .is_ok_and(|boards| boards.detected(slot.workspace_id)),
                );
            }
            previous = Some(slot.workspace_id);
        }
    }

    /// Split the decorated strip between the titlebar and the lower regions'
    /// own bars.
    ///
    /// Tabs of a region below the window's top row go to that region's
    /// [`RegionBarData`] (stored on `self`). Everything else stays in the
    /// titlebar, and the returned addresses record which tab each titlebar
    /// position renders so titlebar events can be resolved back.
    ///
    /// "Active" is the session the *region* is showing, taken from the shell
    /// for every bar alike: the shell owns what is painted, so reading it here
    /// keeps one underline per region and cannot drift from the panes.
    fn partition_tab_strip(
        &mut self,
        data: Vec<TabData>,
        cx: &App,
    ) -> (Vec<TabData>, Vec<TabAddress>) {
        let viewport = self.pane_viewport();
        let badge_palette = &self.config.config().config.workspaces.badge_colors;
        let mut bars: Vec<RegionBarData> = self
            .shell
            .region_bar_rects(viewport, cx)
            .into_iter()
            .map(|(workspace_id, _)| {
                let accent = self.shell.workspace_accent(workspace_id, cx);
                RegionBarData {
                    workspace_id,
                    accent,
                    badge: Self::workspace_group_badge(
                        self.workspace_name(workspace_id).as_deref(),
                        badge_palette,
                        accent,
                        self.shared
                            .beads_boards
                            .lock()
                            .is_ok_and(|boards| boards.detected(workspace_id)),
                    ),
                    tabs: Vec::new(),
                }
            })
            .collect();
        let mut titlebar_data = Vec::new();
        let mut titlebar_slots = Vec::new();
        for (mut tab, slot) in data.into_iter().zip(self.rendered_tabs.addresses()) {
            tab.is_active =
                self.shell.region_shown_session(slot.workspace_id) == Some(slot.session_id);
            let Some(bar) = bars.iter_mut().find(|bar| bar.workspace_id == slot.workspace_id)
            else {
                titlebar_data.push(tab);
                titlebar_slots.push(slot);
                continue;
            };
            tab.group_accent = Some(opaque_slot(bar.accent));
            bar.tabs.push((slot.session_id, tab));
        }
        // Logged on shape changes only: this is the scripted oracle for the
        // in-region bars, which paint no other externally observable trace.
        let signature = |set: &[RegionBarData]| {
            set.iter()
                .map(|bar| format!("{}:{}", bar.workspace_id, bar.tabs.len()))
                .collect::<Vec<_>>()
                .join(",")
        };
        let (old, new) = (signature(&self.region_chrome.bars), signature(&bars));
        if old != new {
            let state = if new.is_empty() { "none" } else { new.as_str() };
            tracing::info!(bars = %state, "lower-region tab bars changed");
        }
        self.region_chrome.bars = bars;
        (titlebar_data, titlebar_slots)
    }

    fn sync_tabs(&mut self, cx: &mut Context<Self>) {
        let Ok(tabs) = self.shared.tabs.lock() else { return };
        self.rendered_tabs = tabs.clone();
        drop(tabs);
        let mut data = self.rendered_tabs.to_tab_data();
        let terminal = &self.config.config().config.terminal;
        let ansi = &self.config.config().theme.ansi_colors;
        if let Ok(ai) = self.shared.ai.lock() {
            for (tab, slot) in data.iter_mut().zip(self.rendered_tabs.addresses()) {
                tab.ai_indicator = ai
                    .tracker
                    .tab_indicator_color(slot.session_id, ansi, terminal)
                    .map(opaque_slot);
                tab.context_suffix =
                    tab_context_suffix_for(&ai.tracker, slot.session_id, &self.context_thresholds);
            }
        }
        let (mut titlebar_data, titlebar_slots) = self.partition_tab_strip(data, cx);
        self.apply_group_badges(&mut titlebar_data, &titlebar_slots, cx);
        self.titlebar_slots = titlebar_slots;
        self.titlebar.update(cx, |bar, ctx| {
            if bar.tabs() != titlebar_data {
                bar.set_tabs(titlebar_data, ctx);
            }
        });
    }

    /// Project the focused tab's live pane topology into titlebar chrome.
    ///
    /// The shell is reconciled before this runs each frame, so asynchronous
    /// session retirement and adoption cannot leave the equalize affordance
    /// one topology behind. The titlebar setter is idempotent to avoid turning
    /// this projection into a redraw loop.
    fn sync_equalize_visibility(&mut self, cx: &mut Context<Self>) {
        let show = self.shell.focused_region_pane_count(cx) >= 2;
        self.titlebar.update(cx, |bar, ctx| bar.set_show_equalize(show, ctx));
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
            LayoutAction::Equalize => self.equalize_layout(cx),
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
            LayoutAction::NewTab => {
                let cwd = self.focused_session_cwd();
                self.create_tab(self.creating_workspace(cx), None, None, cwd);
            }
            LayoutAction::NewClaudeTab => {
                self.create_ai_tab(AiProvider::ClaudeCode, AiResumeMode::New, cx);
            }
            LayoutAction::NewClaudeResumeTab => {
                self.create_ai_tab(AiProvider::ClaudeCode, AiResumeMode::Resume, cx);
            }
            LayoutAction::NewCodexTab => {
                self.create_ai_tab(AiProvider::CodexCode, AiResumeMode::New, cx);
            }
            LayoutAction::NewCodexResumeTab => {
                self.create_ai_tab(AiProvider::CodexCode, AiResumeMode::Resume, cx);
            }
            // Every tab shortcut acts on the region the user is in, which the
            // shell owns; the strip is asked for a tab of that region and can
            // no longer answer with a neighbouring region's.
            LayoutAction::NextTab => {
                let workspace_id = self.shell.focused_workspace_id(cx);
                self.switch_tab(move |tabs| tabs.focus_next(workspace_id), cx);
            }
            LayoutAction::PrevTab => {
                let workspace_id = self.shell.focused_workspace_id(cx);
                self.switch_tab(move |tabs| tabs.focus_prev(workspace_id), cx);
            }
            LayoutAction::SelectTab(index) => {
                let workspace_id = self.shell.focused_workspace_id(cx);
                self.switch_tab(move |tabs| tabs.select(workspace_id, index), cx);
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
        self.focused_pane()?.with_terminal(edit)
    }

    /// A handle on the focused pane. `None` when no pane is attached yet.
    fn focused_pane(&self) -> Option<Arc<PaneGrid>> {
        self.pane_for(self.focused_session()?)
    }

    /// A handle on one session's pane, taken with the registry lock released so
    /// the caller's own work never blocks a drained batch bound for another
    /// pane.
    fn pane_for(&self, session_id: SessionId) -> Option<Arc<PaneGrid>> {
        Some(self.shared.panes.lock().ok()?.pane(session_id))
    }

    /// The session the focused pane is showing.
    fn focused_session(&self) -> Option<SessionId> {
        *self.shared.active_session.lock().ok()?
    }

    /// The published render projection for one session's pane.
    ///
    /// Every per-frame read goes through here rather than through the pane's
    /// own lock: a paint must never queue behind a VTE parse.
    fn pane_frame(&self, session_id: SessionId) -> Option<Arc<PaneFrame>> {
        self.shared.panes.lock().ok()?.frame(session_id)
    }

    /// Move the display viewport and repaint.
    ///
    /// A scroll away from the bottom re-evaluates the split-scroll gate, so an
    /// eligible AI pane grows its pinned live region on the very first page-up
    /// rather than a frame later.
    fn scroll_terminal(&mut self, scroll: Scroll, cx: &mut Context<Self>) {
        let Some(session_id) = self.focused_session() else { return };
        self.scroll_session(session_id, scroll, cx);
    }

    /// Move a named pane's viewport and repaint.
    ///
    /// Split out of [`Self::scroll_terminal`] because the wheel scrolls the
    /// pane under the pointer, focused or not — the scroll chords still act on
    /// the focused pane, and both land here so the split-scroll gate and the
    /// scrollbar pulse cannot drift apart between them.
    fn scroll_session(&mut self, session_id: SessionId, scroll: Scroll, cx: &mut Context<Self>) {
        let Some(pane) = self.pane_for(session_id) else { return };
        let Some((moved, offset, pin_rows)) = pane.with_terminal(|terminal| {
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
        tracing::info!(
            session = %session_id,
            ?scroll,
            moved,
            offset,
            pin_rows,
            "terminal scrollback moved"
        );
        // Pulse even when the viewport did not move: a page-up that hit the top
        // of scrollback is exactly when the user wants to see where they are.
        self.pulse_scrollbar(session_id);
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
        let Some(session_id) = self.focused_session() else { return };
        let Some(pane) = self.pane_for(session_id) else { return };
        // The marks are taken *inside* the pane's lock, the order the drain
        // takes them in when it anchors a mark against the grid it just
        // advanced. Taking them the other way round here would be the one
        // inversion able to deadlock a jump against a firehosed pane.
        let jumped = pane.with_terminal(|terminal| {
            let Ok(marks) = self.shared.prompt_marks.lock() else {
                tracing::warn!("prompt-mark mutex poisoned; dropping jump");
                return None;
            };
            let viewport_top_abs = terminal.viewport_top_abs();
            let total = marks.marks(session_id).len();
            let Some(target) = pick(&marks, session_id, viewport_top_abs) else {
                // FR-011: no candidate is a no-op, not a jump to the nearest
                // thing.
                tracing::info!(
                    action,
                    marks = total,
                    viewport_top_abs,
                    "prompt jump found no mark"
                );
                return None;
            };
            // Landing on a mark is a deliberate scroll, so the pin is cleared
            // for the same reason `scroll_terminal` clears it on
            // `Scroll::Bottom`.
            terminal.set_split_scroll_eligibility(SplitScrollEligibility::default());
            let moved = terminal.scroll_to_abs(target);
            Some((target, moved, terminal.display_offset(), total))
        });
        let Some(Some((target, moved, offset, total))) = jumped else { return };
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
        self.font = zoomed_font(&appearance, self.zoom);
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
    /// spawn a separate settings binary and hand focus over a Unix
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
        // A server-forwarded action has no visible pane focus to inherit. Keep
        // its cwd unset even when this window still remembers the last focused
        // session, so the server's home guard decides the launch directory.
        let automated_ai = match (&action, origin) {
            (AutomationAction::NewClaudeTab, ActionOrigin::Server) => {
                Some((AiProvider::ClaudeCode, AiResumeMode::New))
            }
            (AutomationAction::NewClaudeResumeTab, ActionOrigin::Server) => {
                Some((AiProvider::ClaudeCode, AiResumeMode::Resume))
            }
            (AutomationAction::NewCodexTab, ActionOrigin::Server) => {
                Some((AiProvider::CodexCode, AiResumeMode::New))
            }
            (AutomationAction::NewCodexResumeTab, ActionOrigin::Server) => {
                Some((AiProvider::CodexCode, AiResumeMode::Resume))
            }
            _ => None,
        };
        if let Some((provider, resume_mode)) = automated_ai {
            self.create_ai_tab_request(self.creating_workspace(cx), provider, resume_mode, None);
            return;
        }
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
            AutomationAction::FocusSession { session_id } => {
                self.activate_session_tab(session_id, cx);
            }
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
            ContextMenuAction::OpenFile(path) => {
                url_detect::open_path(&path, self.focused_cwd().as_deref());
            }
            // An explicit user-initiated run, not a clipboard paste: it bypasses
            // the paste-confirmation gate exactly as the legacy client does.
            ContextMenuAction::RunCommand(command) => {
                self.send_key_bytes(format!("{command}\n").into_bytes());
            }
            ContextMenuAction::SendText(text) => self.send_key_bytes(text.into_bytes()),
            ContextMenuAction::RunCommandInWindow(command) => {
                self.create_tab(
                    self.creating_workspace(cx),
                    Some(shell_command_argv(&command)),
                    None,
                    None,
                );
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
        let Some(bounds) = self.focused_grid_bounds() else {
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
        // Only a press that actually began a selection may extend one: a
        // Ctrl+click that opened a link holds the button down over the grid too,
        // and dragging afterwards must not paint a selection nothing started.
        if self.pointer.drag != GridDrag::Selecting {
            return;
        }
        let Some(bounds) = self.focused_grid_bounds() else {
            return;
        };
        let Some(cell) = cell_at(bounds, &self.font, position) else {
            return;
        };
        self.with_focused_grid(|terminal| terminal.extend_selection(cell));
        cx.notify();
    }

    /// Settle a selection drag: copy-on-select publishes the result to the
    /// system clipboard and, on Linux, the primary selection.
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
        let text = clipboard_cleanup::prepare_copy_text(&raw, options);
        self.write_clipboard(text);
        #[cfg(target_os = "linux")]
        clipboard::set_primary(&mut self.clipboard.handle, &raw, options);
        cx.notify();
    }

    /// The focused pane's selection projected onto the painted viewport.
    ///
    /// Served from the published projection, so the spans and the cells they
    /// mark come out of the same grid state and neither read waits on a parse.
    fn selection_spans(&self) -> Vec<SelectionSpan> {
        self.focused_session()
            .and_then(|session_id| self.pane_frame(session_id))
            .map(|frame| frame.selection_spans.clone())
            .unwrap_or_default()
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

    /// The same modes for a named pane, which the wheel needs because it acts
    /// on the pane under the pointer rather than on the focused one.
    fn session_mouse_modes(&self, session_id: SessionId) -> MouseModes {
        self.pane_for(session_id)
            .and_then(|pane| pane.with_terminal(|terminal| terminal.mouse_modes()))
            .unwrap_or_default()
    }

    /// The grid cell under `position` in the `(col, row)` viewport coordinates
    /// a mouse report carries, or `None` when the pointer is off the grid.
    fn report_cell(&self, position: Point<gpui::Pixels>) -> Option<(u16, u16)> {
        self.report_cell_in(self.focused_grid_bounds()?, position)
    }

    /// Where one pane painted its grid last frame.
    fn pane_grid_bounds(&self, session_id: SessionId) -> Option<Bounds<Pixels>> {
        self.pane_bounds.get(&session_id)?.get()
    }

    /// The sink a pane's grid canvas writes its rect into each frame. A pane
    /// with no session yet gets a throwaway cell nothing ever reads.
    fn pane_bounds_sink(&self, session_id: Option<SessionId>) -> GridBounds {
        session_id
            .and_then(|session| self.pane_bounds.get(&session))
            .map_or_else(GridBounds::default, Rc::clone)
    }

    /// The same for the focused pane, which is what every gesture a *click*
    /// precedes resolves against — selection, links, smart selection and the
    /// split-scroll chip all run after [`Self::press_focuses_pane`] has made
    /// the pane under the pointer the focused one.
    fn focused_grid_bounds(&self) -> Option<Bounds<Pixels>> {
        self.pane_grid_bounds(self.focused_session()?)
    }

    /// The session whose painted grid contains `position`.
    ///
    /// The wheel and the overlay scrollbar are pointer gestures with no click
    /// in front of them, so they resolve their pane here rather than reading
    /// the focus. Panes never overlap, so the first rect that contains the
    /// point is the only one that can.
    fn pane_at(&self, position: Point<Pixels>) -> Option<SessionId> {
        self.pane_bounds
            .iter()
            .find(|(_, sink)| sink.get().is_some_and(|rect| rect.contains(&position)))
            .map(|(session_id, _)| *session_id)
    }

    /// The same, against an arbitrary pane's painted grid rect.
    fn report_cell_in(
        &self,
        bounds: Bounds<Pixels>,
        position: Point<gpui::Pixels>,
    ) -> Option<(u16, u16)> {
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
        let Some(session_id) = self.focused_session() else { return };
        self.send_pty_bytes_to(session_id, kind, bytes);
    }

    /// The same, addressed to a named pane: the wheel reports against whichever
    /// pane the pointer is over, which is not necessarily the focused one.
    fn send_pty_bytes_to(&self, session_id: SessionId, kind: &'static str, bytes: Vec<u8>) {
        if bytes.is_empty() {
            return;
        }
        if self.shared.share.lock().is_ok_and(|share| share.is_viewer()) {
            return;
        }
        // The escaped payload is the scripted E2E's oracle: it is the only way
        // to tell a wired encoder from an unwired one without reading the wire.
        tracing::info!(kind, bytes = %escape_report_bytes(&bytes), "mouse input forwarded");
        if let Err(error) = self.sink.key_input(session_id, bytes, false) {
            tracing::warn!(%error, kind, "mouse input refused");
            self.report_refused_input(error);
        }
    }

    /// Forward a button press to a mouse-tracking application.
    ///
    /// Returns `true` when the application claimed the press, so the caller
    /// leaves the client's own gesture — selection, primary-selection paste,
    /// context menu — alone.
    fn forward_mouse_press(&mut self, event: &MouseDownEvent) -> bool {
        if self.card_drag_owns_pointer() {
            return true;
        }
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
        if self.card_drag_owns_pointer() {
            self.pointer.report_button = None;
            self.pointer.report_cell = None;
            return true;
        }
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
        if self.card_drag_owns_pointer() {
            return true;
        }
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

    /// A lifted board card consumes every pointer phase before terminal mouse
    /// reporting. Treat a poisoned store as owned rather than leak input.
    fn card_drag_owns_pointer(&self) -> bool {
        self.shared.beads_boards.lock().map_or(true, |boards| boards.blocks_pty_mouse())
    }

    /// Route one wheel event to whichever of the three consumers claims it.
    ///
    /// A mouse-tracking application gets a button 64 / 65 report, an alternate
    /// screen that asked for alternate scroll (1007) gets cursor keys, and
    /// anything else moves this client's own scrollback viewport.
    ///
    /// `session_id` is the pane the pointer is over rather than the focused
    /// one, because the wheel is a pointer gesture: hovering a pane is enough
    /// to scroll it, the way every other tiling application behaves, and doing
    /// so must not steal focus from the pane the keyboard is in. The button
    /// 64 / 65 coordinates are resolved against that same pane's painted rect.
    fn scroll_pane(
        &mut self,
        session_id: SessionId,
        event: &ScrollWheelEvent,
        cx: &mut Context<Self>,
    ) {
        let natural = self.config.config().config.terminal.scroll.natural_scroll;
        let rows = mouse_reporting::wheel_lines(event.delta, self.font.line_height, natural);
        if rows == 0 {
            return;
        }
        let modes = self.session_mouse_modes(session_id);
        let action = mouse_reporting::wheel_action(modes);
        tracing::info!(rows, ?action, "mouse wheel");
        match action {
            WheelAction::Report => {
                let Some((col, row)) = self
                    .pane_grid_bounds(session_id)
                    .and_then(|bounds| self.report_cell_in(bounds, event.position))
                else {
                    return;
                };
                let bytes = mouse_reporting::encode_mouse_scroll(
                    ScrollDirection::from_rows(rows),
                    col,
                    row,
                    event.modifiers,
                    modes.encoding,
                );
                self.send_pty_bytes_to(session_id, "scroll", bytes);
            }
            WheelAction::CursorKeys => {
                let bytes = mouse_reporting::alternate_scroll_keys(rows);
                self.send_pty_bytes_to(session_id, "alternate-scroll", bytes);
            }
            WheelAction::Scrollback => self.scroll_session(session_id, Scroll::Delta(rows), cx),
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
        let session_count = self.shared.tabs.lock().map_or(0, |tabs| tabs.len());
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
        request_permanent_window_close(&self.shared.lifecycle, &self.sink);
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
                    data.prompts.latest_prompt.clone().or_else(|| data.prompts.first_prompt.clone())
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
        let workspace_id =
            self.shared.tabs.lock().ok().and_then(|tabs| tabs.workspace_of(session_id));
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
        self.activate_session_tab(session_id, cx);
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
    fn poll_window_lifecycle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Ok(mut ai) = self.shared.ai.lock()
            && ai.tracker.clear_stale_processing()
        {
            cx.notify();
        }
        if self.shared.beads_boards.lock().is_ok_and(|mut boards| boards.expire_hover()) {
            cx.notify();
        }
        self.poll_bells(cx);
        self.poll_notifications(cx);
        self.poll_notification_clicks(cx);
        self.poll_beads_writes(cx);
        let process_shutdown = self.shared.process_shutdown.requested();
        let exit = process_shutdown
            .then_some(ExitReason::QuitRequested)
            .or_else(|| self.shared.lifecycle.lock().ok().and_then(|mut l| l.take_exit()));
        if let Some(reason) = exit {
            // Toasts outlive the process that raised them, and a fresh client
            // cannot manage ids it never allocated — so the dispatcher closes
            // every live one before the window goes away.
            self.shutdown_notifications();
            self.finish_exit(reason, window, cx);
            return;
        }
        self.report_focus();
        self.poll_sibling_windows(cx);
        self.poll_window_list();
        self.poll_beads_board(window);
        self.poll_lan_approval(cx);
        self.poll_clipboard(cx);
        self.poll_remote_actions(cx);
        self.poll_restore(cx);
    }

    fn poll_beads_writes(&mut self, cx: &mut Context<Self>) {
        let mut board_refreshes = Vec::new();
        let expired = self.shared.beads_panels.lock().is_ok_and(|mut panels| {
            let expired = panels.expire_writes();
            while let Some(workspace_id) = panels.take_board_refresh() {
                board_refreshes.push(workspace_id);
            }
            expired
        });
        for workspace_id in board_refreshes {
            request_beads_board_or_log(&self.sink, workspace_id, "issue write convergence");
        }
        if expired {
            cx.notify();
        }
    }

    fn finish_exit(&mut self, reason: ExitReason, window: &mut Window, cx: &mut Context<Self>) {
        match reason {
            ExitReason::QuitRequested => {
                if self.process_shutdown_finished {
                    return;
                }
                self.process_shutdown_finished = true;
                tracing::info!("graceful process shutdown requested — flushing window");
                // A quit ends the client, not its server-owned sessions. Flush
                // the window layout and geometry so the next launch restores it.
                self.flush_geometry_now();
                self.flush_snapshot_now(cx);
                if self.shared.process_shutdown.finish_view() {
                    cx.quit();
                } else {
                    window.remove_window();
                }
            }
            ExitReason::WindowClosed => {
                tracing::info!("window close acknowledged by server — closing this window");
                if !self.process_shutdown_finished {
                    self.process_shutdown_finished = true;
                    self.shared.process_shutdown.finish_view();
                }
                self.clear_restore_state(true);
                // GPUI quits on its own once the last hosted window is gone.
                window.remove_window();
            }
        }
    }

    /// Reopen the windows `Welcome` reported the server still holds sessions
    /// for.
    ///
    /// The reader parked them because opening a window is foreground work. Only
    /// the bootstrap connection ever parks any (see
    /// [`ReaderCtx::fan_out_other_windows`]), so a window opened from here never
    /// fans out again and the count can never compound.
    fn poll_sibling_windows(&mut self, cx: &mut Context<Self>) {
        let siblings = self
            .shared
            .lifecycle
            .lock()
            .map(|mut lifecycle| lifecycle.take_sibling_windows())
            .unwrap_or_default();
        if siblings.is_empty() {
            return;
        }
        // The server named windows it still holds sessions for, so this is not a
        // cold restart. The snapshots those windows also have on disk describe
        // the same windows, and fanning them out as `--restore-child` processes
        // would open every one of them twice.
        self.restore.siblings = 0;
        for window_id in siblings {
            self.open_restored_window(window_id, cx);
        }
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

    /// Advance the focused cursor's 530 ms blink phase and invalidate only
    /// when a visible edge changes.
    fn tick_cursor_blink(&mut self, cx: &mut Context<Self>) {
        let enabled = self.config.config().config.appearance.cursor_blink;
        if !enabled {
            if self.focus.cursor_blink.show_now() {
                cx.notify();
            }
            return;
        }
        if !self.focus.cursor_blink.window_active
            || self.focus.cursor_blink.last_toggle.elapsed() < CURSOR_BLINK_INTERVAL
        {
            return;
        }
        self.focus.cursor_blink.visible = !self.focus.cursor_blink.visible;
        self.focus.cursor_blink.last_toggle = Instant::now();
        cx.notify();
    }

    // -- Cold-restart restore and window geometry persistence -----------------

    /// Run the restore machinery for one tick: adopt the window id, replay a
    /// claimed snapshot once the server has answered, and flush whichever of the
    /// two persisted files has settled.
    fn poll_restore(&mut self, cx: &mut Context<Self>) {
        if self.restore.window_id.is_none() {
            let assigned =
                self.shared.lifecycle.lock().ok().and_then(|lifecycle| lifecycle.window_id());
            if let Some(window_id) = assigned {
                self.restore.adopt_assigned_window(window_id);
                self.adopt_assigned_geometry(window_id, cx);
            }
        }
        self.sync_launch_bindings();
        self.replay_cold_restart(cx);
        self.flush_geometry_if_due();
        if self.restore.layout_dirty_since.is_some_and(|at| at.elapsed() >= RESTORE_DEBOUNCE) {
            self.flush_snapshot_now(cx);
        }
    }

    /// Read the geometry record of the window the server just named, for a
    /// process that opened without one.
    ///
    /// A launch that found no claimable snapshot sends an unnamed `Hello`, so
    /// it opens at the default size on the active monitor and only learns from
    /// `Welcome` that it adopted an existing window — by which time it is
    /// already on screen. Without this, the first flush wrote that default over
    /// the adopted window's saved record and its real bounds were gone. The
    /// record is taken as the baseline instead: its position is re-asserted
    /// like any restore's, and the placement is held open so no capture taken
    /// on the way there is persisted over the record it is aiming at.
    fn adopt_assigned_geometry(&mut self, window_id: WindowId, cx: &mut Context<Self>) {
        if self.restore.assigned_geometry == AssignedGeometry::Adopted {
            return;
        }
        self.restore.assigned_geometry = AssignedGeometry::Adopted;
        let Some(geometry) = saved_geometry_for(window_id) else { return };
        tracing::info!(%window_id, "adopting the assigned window's saved geometry");
        let level = geometry.zoom;
        if let Ok(mut boards) = self.shared.beads_boards.lock() {
            boards.restore_pins(geometry.beads_pinned.iter().copied());
        }
        self.restore.adopt_geometry_record(geometry);
        // This process built its font before it knew which window it was
        // holding, so the record's zoom level is applied here rather than at
        // construction. Without it the next capture would write level 0 over
        // the level the adopted window was left at.
        if level != self.zoom.level() {
            self.apply_zoom(move |zoom| *zoom = ZoomState::at_level(level), cx);
        }
        // Unlike a seeded restore this window is NOT at the record's bounds, so
        // the placement stays open even when there is no position to re-assert:
        // it is what keeps the opening default off the record.
        self.restore.placement = RestorePlacement::Restoring;
        self.restore.geometry_dirty_since = None;
    }

    /// Reconcile one live session with its retained launch metadata.
    fn sync_launch_binding(&mut self, session_id: SessionId, retained: RetainedSessionMetadata) {
        let (ai, cwd, launch_id, cleared) = retained;
        if let Some(binding) = self.restore.bindings.get_mut(&session_id) {
            if reconcile_retained_binding(binding, ai.as_ref(), cwd.as_deref(), cleared) {
                self.restore.mark_layout_dirty();
            }
            return;
        }

        let mut binding = self.restore.requested.pop_front().unwrap_or_else(|| {
            retained_session_binding(retained_ai_ref(ai.as_ref()), cwd.clone(), launch_id)
        });
        reconcile_retained_binding(&mut binding, ai.as_ref(), cwd.as_deref(), cleared);
        self.restore.bindings.insert(session_id, binding);
        self.restore.mark_layout_dirty();
    }

    /// Give every live session a launch binding and forget the ones that ended.
    ///
    /// A binding is what a snapshot's `LaunchRecord` is built from, so this is
    /// the thing that decides what a cold restart relaunches. Sessions this
    /// window asked for take the binding queued by the request that asked for
    /// them, in the FIFO order the one ordered writer channel guarantees.
    /// Everything else (a reattach, a share join) is hydrated from the
    /// server-retained AI and CWD metadata already folded into shared chrome.
    /// Existing bindings are reconciled too, so each live `AiStateChanged` or
    /// `CwdChanged` edge makes the next snapshot current without changing its
    /// launch identity.
    fn sync_launch_bindings(&mut self) {
        let Ok(tabs) = self.shared.tabs.lock() else { return };
        let live: Vec<SessionId> = tabs.entries().map(|tab| tab.session_id).collect();
        drop(tabs);
        let retained: HashMap<SessionId, RetainedSessionMetadata> = {
            let ai = self.shared.ai.lock().ok();
            let chrome = self.shared.chrome_metadata.lock().ok();
            live.iter()
                .map(|session_id| {
                    let provider =
                        ai.as_ref().and_then(|ai| ai.tracker.provider_for_session(*session_id));
                    let conversation_id =
                        ai.as_ref().and_then(|ai| ai.conversations.get(session_id).cloned());
                    let metadata =
                        chrome.as_ref().and_then(|metadata| metadata.session(*session_id));
                    let cwd = metadata.and_then(|session| session.cwd.clone());
                    let launch_id = metadata.and_then(|session| session.launch_id.clone());
                    let cleared =
                        ai.as_ref().is_some_and(|ai| ai.binding_cleared.contains(session_id));
                    (
                        *session_id,
                        (
                            provider.map(|provider| (provider, conversation_id)),
                            cwd,
                            launch_id,
                            cleared,
                        ),
                    )
                })
                .collect()
        };
        if !live.is_empty()
            && let Ok(mut pending) = self.shared.initial_session.binding.lock()
            && let Some(binding) = pending.take()
        {
            self.restore.requested.push_front(binding);
        }
        for session_id in &live {
            let metadata = retained.get(session_id).cloned().unwrap_or_default();
            self.sync_launch_binding(*session_id, metadata);
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
    /// [`RestoreRuntime::adopt_assigned_window`] has already dropped a claim
    /// `Welcome` refused. Three gates remain, in order. The server must have
    /// *answered* the startup `ListSessions` — an unanswered list is not an
    /// empty one. That answer must be empty: a server that kept this window's
    /// sessions is restored by the ordinary reattach, and replaying on top of
    /// it would double every pane. And the window must have painted once,
    /// because restored panes are sized from the measured grid area; creating
    /// them earlier would spawn every PTY at the fallback 80x24.
    fn replay_cold_restart(&mut self, cx: &mut Context<Self>) {
        if self.restore.pending.is_none() {
            return;
        }
        let live = self.shared.tabs.lock().map_or(0, |tabs| tabs.len());
        let disposition = restore_claim_disposition(
            self.restore.claimed_window,
            self.restore.window_id,
            self.shared.session_list_seen.load(Ordering::Acquire),
            live,
        );
        match disposition {
            RestoreClaimDisposition::Waiting => return,
            RestoreClaimDisposition::Warm => tracing::info!(
                live,
                "server kept this window's sessions — skipping the cold-restart replay"
            ),
            RestoreClaimDisposition::Cold => {}
        }
        if disposition != RestoreClaimDisposition::Cold {
            // The claimed snapshot file stays on disk (claim is
            // non-destructive) because this window never consumed it.
            self.restore.pending = None;
            // Saving this same assigned id supersedes its index claim; there
            // is no different snapshot to retire.
            self.restore.claimed_window = None;
            self.restore.siblings = 0;
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
        self.restore.restored_prompts = rebuilt
            .panes
            .iter()
            .filter(|(_, pane)| pane.prompts.prompts.prompt_count > 0)
            .map(|(pane_id, pane)| {
                let restored = RestoredPrompts {
                    prompts: pane.prompts.clone(),
                    conversation_id: pane.last_conversation_id.clone(),
                };
                (*pane_id, restored)
            })
            .collect();
        let launches = self.shell.adopt_restored(rebuilt, cx);
        let viewport = self.pane_viewport();
        let placements = self.shell.placements(viewport, cx);
        let split = placements.len() > 1;
        let sizes: HashMap<PaneId, TerminalSize> = placements
            .into_iter()
            .map(|placement| {
                let rect = self.painted_pane_rect(&placement, split);
                (placement.pane_id, self.grid_size_for(rect))
            })
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
        // Only now is the server known to have lost its sessions, which is the
        // one case the remaining snapshots are the truth about the user's other
        // windows. A server that kept them hands them back through `Welcome`
        // instead, and fanning out here as well would double every window.
        restore_replay::spawn_restore_children(std::mem::take(&mut self.restore.siblings));
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
        let result = self.sink.create_session(SessionLaunch {
            workspace_id: launch.workspace_id,
            size,
            cwd: launch.cwd.clone(),
            command: launch.session_launch.command.clone(),
            ai_launch: launch.session_launch.ai_launch.clone(),
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
        // Cloned rather than held: the IPC reader writes this mutex on every
        // `PromptReceived`, and the snapshot walk below is long enough that
        // holding the lock across it would stall the reader. A poisoned mutex
        // costs this snapshot its prompt rows, never the snapshot itself.
        let prompts = self.shared.ai.lock().map(|ai| ai.prompts.clone()).unwrap_or_default();
        let snapshot = self.shell.restore_snapshot(window_id, &self.restore.bindings, &prompts, cx);
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
        // Only now that a replacement snapshot is durably on disk may the
        // claimed pre-restart snapshot be retired; until this point it stayed
        // behind as the last good layout.
        if let Some(claimed) = self.restore.claimed_window.take()
            && claimed != window_id
        {
            tracing::info!(%claimed, "fresh snapshot written; retiring the claimed snapshot");
            if let Err(error) = self.restore.store.remove_from_index(claimed) {
                tracing::warn!(%error, "failed to drop the claimed snapshot from the index");
            }
            self.restore.store.remove_window(claimed);
        }
    }

    /// Drop this window's snapshot and index entry.
    fn forget_restore_entry(&self, window_id: WindowId) {
        if let Err(error) = self.restore.store.remove_from_index(window_id) {
            tracing::warn!(%error, "failed to remove the window from the restore index");
        }
        self.restore.store.remove_window(window_id);
    }

    /// Clear the persisted restore state when this window is destroyed.
    ///
    /// Only a `CloseWindow` reaches here: the user asked for this window and its
    /// sessions to be gone, so nothing about it should survive — including its
    /// size, hence `drop_geometry`. A quit is the opposite (the sessions live
    /// on) and flushes instead, and a crash leaves whatever was last flushed.
    fn clear_restore_state(&mut self, drop_geometry: bool) {
        self.restore.cleared = true;
        self.restore.layout_dirty_since = None;
        self.restore.geometry_dirty_since = None;
        // A deliberate exit also retires the claimed pre-restart snapshot:
        // the user is saying none of these panes should come back.
        if let Some(claimed) = self.restore.claimed_window.take() {
            self.forget_restore_entry(claimed);
        }
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

    fn poll_beads_board(&mut self, window: &Window) {
        if let Some(workspace_id) = self
            .shared
            .beads_boards
            .lock()
            .ok()
            .and_then(|mut boards| boards.due_retry(BEADS_UNAVAILABLE_RETRY_INTERVAL))
        {
            request_beads_board_or_log(&self.sink, workspace_id, "unavailable retry");
        }
        if !window.is_window_active() || self.last_beads_poll.elapsed() < BEADS_PINNED_POLL_INTERVAL
        {
            return;
        }
        // Every pinned board is open on screen, so every one of them polls.
        let pinned =
            self.shared.beads_boards.lock().map(|boards| boards.pinned()).unwrap_or_default();
        if pinned.is_empty() {
            return;
        }
        self.last_beads_poll = Instant::now();
        for workspace_id in pinned {
            if let Err(error) = self.sink.request_beads_board(workspace_id) {
                tracing::warn!(%error, "pinned Beads board poll dropped: IPC writer closed");
            }
        }
    }

    /// Ask the server for a new session in the active workspace.
    ///
    /// The tab appears once the server answers with `SessionCreated`, so the
    /// strip never shows a tab whose PTY failed to spawn.
    ///
    /// The region is the caller's to name: the shell owns which region has the
    /// window's focus, and asking the strip for it — as this did — meant a
    /// second copy of that fact could disagree and file the new tab in a region
    /// the user was not looking at.
    fn create_tab(
        &mut self,
        workspace_id: Option<WorkspaceId>,
        command: Option<Vec<String>>,
        ai_launch: Option<AiLaunchSpec>,
        cwd: Option<PathBuf>,
    ) {
        let Some(workspace_id) = workspace_id else {
            tracing::warn!("new tab ignored: no workspace is attached yet");
            return;
        };
        // Queued before the request goes out so the answering `SessionCreated`
        // is bound to what this tab actually launched — a plain shell, or the
        // AI command an AI-tab shortcut asked for, which is what a cold restart
        // has to relaunch rather than a bare login shell. The binding's launch
        // id rides along as the env-envelope id so the session persists its
        // environment under the same id the next snapshot will point at.
        let binding = launch_binding_for(command.as_ref(), ai_launch.as_ref(), cwd.clone());
        let launch_id = binding.launch_id.clone();
        self.restore.requested.push_back(binding);
        let result = self.sink.create_session(SessionLaunch {
            workspace_id,
            size: self.focused_pane_size,
            cwd,
            command,
            ai_launch,
            launch_id,
        });
        if let Err(error) = result {
            tracing::warn!(%error, "new tab dropped: IPC writer closed");
        }
    }

    /// Ask for an AI tab using structured intent.
    ///
    /// An AI tab always starts at the focused region's project root when there
    /// is one. An AI session is scoped to a project rather than to wherever a
    /// shell happens to have wandered, so a `cd` deep into a subtree should not
    /// change where the assistant is rooted. The server only reports a project
    /// root while the region's CWD is under a configured `workspaces.roots`
    /// entry and clears it on the way out, so "in a workspace" needs no
    /// separate test — outside one, the tab falls back to the focused pane's
    /// CWD exactly like a plain new tab.
    ///
    /// A missing pane CWD too (no focused session, an automation action without
    /// visible focus, or a shell that never emitted OSC 7) stays `None` and
    /// leaves the server's directory validation and `$HOME` fallback as the
    /// final guard. Cold-restart replay does not come through here; it keeps
    /// the persisted `LaunchRecord.cwd` passed by
    /// [`Self::dispatch_replay_launch`].
    fn create_ai_tab(&mut self, provider: AiProvider, resume_mode: AiResumeMode, cx: &App) {
        let cwd =
            self.shell.focused_workspace_project_root(cx).or_else(|| self.focused_session_cwd());
        self.create_ai_tab_request(self.creating_workspace(cx), provider, resume_mode, cwd);
    }

    /// Build one immutable structured AI create request.
    fn create_ai_tab_request(
        &mut self,
        workspace_id: Option<WorkspaceId>,
        provider: AiProvider,
        resume_mode: AiResumeMode,
        cwd: Option<PathBuf>,
    ) {
        let launch = restore_replay::ai_launch_values(provider, resume_mode, None);
        self.create_tab(workspace_id, launch.command, launch.ai_launch, cwd);
    }

    /// Read the focused session's last server-reported OSC 7 directory.
    fn focused_session_cwd(&self) -> Option<PathBuf> {
        let session_id = self.shared.active_session.lock().ok().and_then(|guard| *guard)?;
        self.shared.chrome_metadata.lock().ok()?.session(session_id)?.cwd.clone()
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
        let mut adopted = false;
        if let Some((workspace_id, pane)) = self.shell.pane_for_session(session_id, cx) {
            self.shell.focus_pane(workspace_id, pane, cx);
        } else if let Some((workspace_id, pane)) = self.tab_adoption_pane(session_id, cx) {
            self.adopt_session(pane, session_id);
            self.shell.focus_pane(workspace_id, pane, cx);
            adopted = true;
        }
        // Resolve the incoming session's grid only after it occupies its pane:
        // prompt chrome is per tab, so the outgoing pane size is not a valid
        // attach fallback. Attach still precedes the publish below, so any
        // remaining Resize + RequestSnapshot stays authorised on the ordered
        // channel.
        self.attach(session_id, cx);
        if adopted {
            self.publish_pane_sizes(cx);
        }
        // A tab switch moves the focused pane, which the server relays to PTY
        // applications as a CSI focus event — reported here rather than on the
        // next tick so the switched-to pane learns about it immediately.
        self.report_focus();
        self.sync_tabs(cx);
        // It also changes which tab the region is showing, and the reported
        // tree is where that is persisted: a reconnect restores the region to
        // its `active_tab_index`, so a switch that is never reported comes back
        // as whichever tab was active the last time something else changed.
        // The report is deduplicated against the last one, so an idle switch
        // back and forth costs one frame's tree build and no traffic.
        self.report_workspace_tree(cx);
    }

    /// The region and pane an unshown tab should be shown in.
    ///
    /// A tab belongs to its own region, so it lands in *that* region's focused
    /// pane — never the window's, which routinely sits elsewhere and would paint
    /// the session into a region whose bar never listed it while displacing that
    /// region's own tab. This is the one adoption rule; the cross-workspace
    /// guard it replaces existed because a window-global selection could name a
    /// tab of any region, and "resolved" that by re-pointing the strip at the
    /// focused pane — overriding the user's own click.
    ///
    /// A tab whose workspace has no region in this window (a reattach after
    /// reconnect, a strip that outlived its region) has nowhere of its own to
    /// go and falls back to the focused pane.
    fn tab_adoption_pane(&self, session_id: SessionId, cx: &App) -> Option<(WorkspaceId, PaneId)> {
        let own_region = self
            .shared
            .tabs
            .lock()
            .ok()
            .and_then(|tabs| tabs.workspace_of(session_id))
            .filter(|workspace_id| self.shell.has_region(*workspace_id))
            .and_then(|workspace_id| {
                Some((workspace_id, self.shell.region_focused_pane(workspace_id)?))
            });
        own_region
            .or_else(|| Some((self.shell.focused_workspace_id(cx), self.shell.focused_pane(cx)?)))
    }

    /// Hand keyboard focus back to the terminal after a pointer tab action.
    ///
    /// The click legitimately focuses the titlebar control first; deferring the
    /// terminal focus until after that event settles matches standard terminal
    /// behavior where a mouse tab switch immediately leaves typing live.
    fn defer_terminal_focus(&self, cx: &mut Context<Self>) {
        let focus = self.focus.root.clone();
        cx.with_window(cx.entity_id(), move |window, cx| {
            window.defer(cx, move |window, cx| {
                window.focus(&focus, cx);
            });
        });
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
    fn attach(&mut self, session_id: SessionId, cx: &App) {
        if let Ok(mut guard) = self.shared.active_session.lock() {
            *guard = Some(session_id);
        }
        self.stream_session(session_id, cx);
    }

    /// Attach and subscribe `session_id` without making it the active session,
    /// so a pane in an unfocused region can stream a freshly adopted tab while
    /// the user keeps typing wherever they are.
    fn stream_session(&mut self, session_id: SessionId, cx: &App) {
        if let Ok(mut attached) = self.shared.attached.lock() {
            attached.insert(session_id);
        }
        // The attach replay is the first frame the switched-to tab paints, so
        // size it from that session's current placement rather than the
        // outgoing focused pane. Prompt chrome is per tab and can change the
        // row count even when both sessions share one pane.
        let size = self
            .placed_pane_size(session_id, cx)
            .or_else(|| self.pane_sizes.get(&session_id).copied())
            .unwrap_or(self.focused_pane_size);
        self.pane_sizes.insert(session_id, size);
        self.resize_pane_grid(session_id, size);
        if self.shell.focused_session(cx) == Some(session_id) {
            self.adopt_focused_pane_size(size);
        }
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
    ///
    /// The focused tab is the session the window is attached to. The strip used
    /// to answer this from its own window-global cursor, which is a second copy
    /// of the same fact and could name a tab in a region the window had since
    /// focused away from.
    fn close_active_tab(&self) {
        let Some(session_id) = self.shared.active_session.lock().ok().and_then(|guard| *guard)
        else {
            return;
        };
        self.close_session(session_id, "close tab dropped");
    }

    /// Close `session_id`, logging the request so UI and shortcut paths share
    /// the same IPC edge and diagnostics.
    fn close_session(&self, session_id: SessionId, warning: &str) {
        if let Err(error) = self.sink.close_session(session_id) {
            tracing::warn!(%error, "{warning}: IPC writer closed");
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
    ///
    /// The id is minted here rather than left to the server. An unnamed `Hello`
    /// is the *restart* claim — the server answers it with one of the windows
    /// whose sessions outlived their client — so a new window that sent one
    /// would open showing a window the user had before, and the real one would
    /// stay unopenable. A fresh UUID is by construction not a window the server
    /// knows, so it is assigned verbatim and the window is genuinely empty.
    fn open_new_window(&mut self, cx: &mut Context<Self>) {
        let terminal_size = self.terminal_size;
        let window_id = WindowId::new();
        let (shared, sink) = start_window_backend(
            terminal_size,
            WindowBackend {
                claim: Some(window_id),
                join_intent: LocalJoinIntent::Plain,
                initial_session: true,
                fan_out: false,
            },
        );
        // A deliberately opened window replays no snapshot and inherits no
        // geometry, then its backend creates its own first login shell.
        open_window(cx, &shared, &sink, terminal_size, ColdStart::for_window(None));
        tracing::info!(%window_id, "opened a new terminal window");
    }

    /// Reopen a window the server still holds sessions for.
    ///
    /// Named in this window's `Welcome` as one of `other_windows`: the user had
    /// it open when the client last exited, the server kept its sessions, and
    /// only one window can be handed back per connection. It claims that exact
    /// id — so it adopts *its own* sessions rather than racing the other
    /// restored windows for them — creates no first shell, and opens at the
    /// geometry that window was last seen at.
    fn open_restored_window(&mut self, window_id: WindowId, cx: &mut Context<Self>) {
        let terminal_size = self.terminal_size;
        let (shared, sink) = start_window_backend(
            terminal_size,
            WindowBackend {
                claim: Some(window_id),
                join_intent: LocalJoinIntent::Plain,
                initial_session: false,
                fan_out: false,
            },
        );
        open_window(cx, &shared, &sink, terminal_size, ColdStart::for_window(Some(window_id)));
        tracing::info!(%window_id, "reopened a window the server kept sessions for");
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
    /// reported its bounds for the first time. It is a *paint* fallback only:
    /// nothing derived from it may reach the server, because the window it
    /// describes is a guess and the PTY it would size is real. Anything that
    /// publishes goes through [`Self::measured_pane_viewport`] instead.
    fn pane_viewport(&self) -> Rect {
        self.measured_pane_viewport().unwrap_or(Rect {
            x: 0.0,
            y: 0.0,
            width: self.font.cell_width() * f32::from(COLUMNS),
            height: self.font.line_height * f32::from(ROWS),
        })
    }

    /// The grid area as the paint pass actually measured it, or `None` before
    /// the measuring canvas has reported a positive rect.
    ///
    /// The publish path takes this rather than [`Self::pane_viewport`]: the
    /// nominal fallback differs from the real band by whatever the chrome
    /// bands take, so publishing it announces a grid the window never had and
    /// then corrects it one frame later. The server saw both, and every
    /// application that had already right-padded a line to the first width
    /// wrapped it against the second.
    fn measured_pane_viewport(&self) -> Option<Rect> {
        let bounds = self.grid_area.get()?;
        let width = f32::from(bounds.size.width);
        let height = f32::from(bounds.size.height);
        (width > 0.0 && height > 0.0).then_some(Rect { x: 0.0, y: 0.0, width, height })
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
        // The fallback rect must never be latched here either: recording it
        // would mark a size as published that never was, and a later real
        // measurement that happened to match it would then be skipped.
        let Some(viewport) = self.measured_pane_viewport() else { return };
        let measured = (viewport.width, viewport.height);
        if self.published_grid_area == Some(measured) {
            return;
        }
        self.published_grid_area = Some(measured);
        self.publish_pane_sizes(cx);
    }

    /// Split the focused pane and ask the server for the session it will host.
    fn split_pane(&mut self, direction: SplitDirection, cx: &mut Context<Self>) {
        // Capture before the split moves focus onto its unbound pending pane.
        let cwd = self.focused_session_cwd();
        if self.shell.split_focused_pane(direction, cx).is_none() {
            tracing::warn!(?direction, "split ignored: the window has no focused pane");
            return;
        }
        tracing::info!(?direction, panes = self.shell.pane_count(cx), "split the focused pane");
        self.request_pane_session(cwd, cx);
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

    /// Reset every workspace-region and pane split to equal space.
    ///
    /// Reached from the `equalize` keybinding, the status-bar balance button
    /// and the titlebar equalize icon; splits and closes re-equalize on their
    /// own.
    fn equalize_layout(&mut self, cx: &mut Context<Self>) {
        self.shell.equalize_all(cx);
        tracing::info!("equalized the window layout");
        self.after_layout_change(cx);
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
        // A new region is a fresh context, not a continuation of the source
        // pane, so it sends no CWD and the server's home fallback wins.
        self.request_pane_session(None, cx);
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

    /// The workspace a newly created session should be filed under.
    ///
    /// The shell's focused region is the answer whenever the server knows it —
    /// it is the region the user is looking at. A `workspace_split_*` opens a
    /// region *before* the server has minted its workspace, though, so the
    /// session seeding that region has to be created through a workspace the
    /// server has heard of; [`Self::follow_session_to_region`] re-files it once
    /// the adopting pane turns out to sit elsewhere.
    fn creating_workspace(&self, cx: &App) -> Option<WorkspaceId> {
        let focused = self.shell.focused_workspace_id(cx);
        if self.shell.is_server_workspace(focused) {
            return Some(focused);
        }
        let attached = self.shared.active_session.lock().ok().and_then(|guard| *guard);
        let tabs = self.shared.tabs.lock().ok()?;
        attached
            .and_then(|session_id| tabs.workspace_of(session_id))
            .or_else(|| tabs.regions().first().map(|region| region.workspace_id))
    }

    /// Ask the server for a session to fill the pane that just appeared.
    ///
    /// The pane was queued by the split, so the reconcile pass hands it the
    /// session as soon as `SessionCreated` lands. `cwd` is captured from the
    /// source pane before the split moves focus to this pending pane.
    fn request_pane_session(&mut self, cwd: Option<PathBuf>, cx: &App) {
        let Some(workspace_id) = self.creating_workspace(cx) else {
            tracing::warn!("pane session ignored: no workspace is attached yet");
            return;
        };
        let binding = launch_binding_for(None, None, cwd.clone());
        let launch_id = binding.launch_id.clone();
        self.restore.requested.push_back(binding);
        let result = self.sink.create_session(SessionLaunch {
            workspace_id,
            size: self.focused_pane_size,
            cwd,
            command: None,
            ai_launch: None,
            launch_id,
        });
        if let Err(error) = result {
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
            tabs.show(session_id);
        }
        self.attach(session_id, cx);
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
        // The strip is the region tab lists the report carries, so it is read
        // once here and handed down rather than reached for per region.
        let tabs = self.shared.tabs.lock().map(|tabs| tabs.clone()).unwrap_or_default();
        let tree = self.shell.wire_tree(&tabs, cx);
        if self.reported_trees.back() == Some(&tree) {
            return;
        }
        if let Err(error) = self.sink.report_workspace_tree(tree.clone()) {
            tracing::warn!(%error, "workspace tree report dropped: IPC writer closed");
            return;
        }
        if self.reported_trees.len() == Self::REPORTED_TREE_HISTORY {
            self.reported_trees.pop_front();
        }
        self.reported_trees.push_back(tree);
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

    /// The terminal grid a *painted* pane rect resolves to at the live font
    /// metrics.
    ///
    /// Shared by the per-frame republish and the cold-restart replay so a
    /// restored PTY is created at exactly the size the pane it lands in will
    /// report one frame later. The arithmetic is
    /// [`grid_for_rect`](scribe_client::restore_replay::grid_for_rect), the
    /// client's one grid formula, so neither path can drift from the other.
    /// Pass the rect the pane paints into — see [`Self::painted_pane_rect`].
    fn grid_size_for(&self, rect: Rect) -> TerminalSize {
        let cell_width = self.font.cell_width();
        let line_height = self.font.line_height;
        let grid = grid_for_rect(rect, (cell_width, line_height));
        TerminalSize {
            cols: grid.cols,
            rows: grid.rows,
            cell_width: round_positive_f32_to_u16(cell_width).max(1),
            cell_height: round_positive_f32_to_u16(line_height).max(1),
        }
    }

    /// The rect a pane actually paints its terminal grid into.
    ///
    /// The placement rect is the pane's *outer* box, and two bands live inside
    /// it that the PTY must not be told about. A split window draws a one-pixel
    /// border on every pane and GPUI insets the child's content box by it, so
    /// the painted grid is two pixels narrower and shorter than the placement.
    /// The prompt strip is pane-internal chrome, so its rows come out of that
    /// pane's own PTY rather than the shared grid band. Reserving neither is
    /// how the client came to report more columns than it renders.
    fn painted_pane_rect(&self, placement: &PanePlacement, split: bool) -> Rect {
        let mut rect = placement.rect;
        if split {
            rect.width = (rect.width - 2.0 * PANE_BORDER_WIDTH).max(0.0);
            rect.height = (rect.height - 2.0 * PANE_BORDER_WIDTH).max(0.0);
        }
        if let Some(session_id) = placement.session_id {
            rect.height = (rect.height - self.pane_prompt_bar_height(session_id)).max(0.0);
        }
        rect
    }

    /// The measured grid currently painted for a shown session.
    ///
    /// Attach and publish both lower the same placement through
    /// [`Self::painted_pane_rect`] and [`Self::grid_size_for`], so the first
    /// replay cannot be sized from the tab that happened to occupy the pane
    /// before it.
    fn placed_pane_size(&self, session_id: SessionId, cx: &App) -> Option<TerminalSize> {
        let viewport = self.measured_pane_viewport()?;
        let placements = self.shell.placements(viewport, cx);
        let split = placements.len() > 1;
        placements
            .iter()
            .find(|placement| placement.session_id == Some(session_id))
            .map(|placement| self.grid_size_for(self.painted_pane_rect(placement, split)))
    }

    /// Tell the server (and each local grid) how big every pane now is.
    ///
    /// A `Resize` alone would leave the pane showing a grid it can no longer
    /// hold, because this client owns no PTY and never reflows locally, so each
    /// changed pane also reshapes its display grid and asks for the
    /// authoritative screen back. Unchanged panes are skipped, so a redraw
    /// storm never turns into a `RequestSnapshot` storm.
    fn publish_pane_sizes(&mut self, cx: &mut Context<Self>) {
        // Before the first paint the only viewport on offer is the synthetic
        // COLUMNS x ROWS fallback, which is not the band this window will have.
        // Say nothing until the canvas has measured one: the probe defers back
        // here the moment it does.
        let Some(viewport) = self.measured_pane_viewport() else { return };
        let cell_width = self.font.cell_width();
        let line_height = self.font.line_height;
        if cell_width <= 0.0 || line_height <= 0.0 {
            return;
        }
        // A pane the reader reattached without announcing a grid
        // ([`reattach_visible_sessions`]) has to be re-published even though
        // nothing about the layout moved, so forget what was last sent for it.
        if let Ok(mut deferred) = self.shared.deferred_grids.lock() {
            for session_id in deferred.drain(..) {
                self.pane_sizes.remove(&session_id);
            }
        }
        let placements = self.shell.placements(viewport, cx);
        let split = placements.len() > 1;
        let live: HashSet<SessionId> =
            placements.iter().filter_map(|placement| placement.session_id).collect();
        self.pane_sizes.retain(|session, _| live.contains(session));
        for placement in placements {
            let size = self.grid_size_for(self.painted_pane_rect(&placement, split));
            if placement.focused {
                self.adopt_focused_pane_size(size);
            }
            let Some(session_id) = placement.session_id else { continue };
            if self.pane_sizes.get(&session_id) == Some(&size) {
                continue;
            }
            self.pane_sizes.insert(session_id, size);
            self.resize_pane_grid(session_id, size);
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

    /// Record the focused pane's grid on both sides of the window.
    ///
    /// A new tab and a split both open into the focused pane, so the size they
    /// announce has to be that pane's, not the window's. The IPC reader attaches
    /// into that same pane from its own thread and can measure nothing, so the
    /// value is mirrored into [`Shared`] rather than kept on the view alone.
    fn adopt_focused_pane_size(&mut self, size: TerminalSize) {
        self.focused_pane_size = size;
        if let Ok(mut focused) = self.shared.focused_size.lock() {
            *focused = size;
        } else {
            tracing::warn!("focused-size mutex poisoned; the reader keeps the old grid");
        }
    }

    /// Reshape one pane's display grid.
    ///
    /// The pane is resolved out of the registry and the registry lock dropped
    /// before the reshape, so a resize waits on at most the batch being parsed
    /// into that one pane rather than on every pane at once.
    fn resize_pane_grid(&self, session_id: SessionId, size: TerminalSize) {
        let Some(pane) = self.pane_for(session_id) else { return };
        pane.with_terminal(|terminal| {
            terminal.resize(usize::from(size.cols), usize::from(size.rows));
        });
    }

    /// Rebuild this window's regions, splits, and pane sessions from the
    /// workspace tree the server shipped with the first `SessionList` of a
    /// (re)connect.
    ///
    /// The server persists each window's tree from `ReportWorkspaceTree` and
    /// ships it back precisely so a freshly started client can rebuild its
    /// splits instead of flattening every session into one region — which is
    /// what an upgrade restart used to do. The parked tree is taken exactly
    /// once and adopted when the shell is still the untouched startup layout
    /// ([`PaneShell::is_unused`]) or when the tree is not one this view
    /// recently reported (a stale claim; see below). Sessions named in the
    /// tree but gone from the list are pruned by the shell, as on the cold
    /// path; sessions in the list but not in the tree stay ordinary tabs.
    ///
    /// Returns `true` when a tree was adopted this frame, so the caller can
    /// hold its retain/close pass until the tab strip catches up.
    fn adopt_server_topology(&mut self, cx: &mut Context<Self>) -> bool {
        let parked = self.shared.server_topology.lock().ok().and_then(|mut slot| slot.take());
        let Some((tree, live)) = parked else { return false };
        if self.restore.replaying {
            tracing::info!("cold-restart replay in flight; ignoring the server workspace tree");
            return false;
        }
        // A live layout wins only while this view is the tree's author: a
        // mid-session redial parks the very tree we last reported (possibly a
        // few queued reports back), so keeping the local layout is keeping the
        // truth. A parked tree we never reported means another client owned
        // and reshaped this window since — a stale claim. Imposing our layout
        // then is how a leftover pre-update client once closed every rebuilt
        // workspace of the reconnecting window; adopt the server's tree
        // instead.
        if !self.shell.is_unused(cx) {
            if self.reported_trees.contains(&tree) {
                tracing::info!("window already has a layout; ignoring the server workspace tree");
                return false;
            }
            tracing::warn!(
                "another client reshaped this window; adopting the server tree over the stale local layout"
            );
        }
        let visible = self.shell.adopt_server_tree(&tree, &live, cx);
        if visible.is_empty() {
            tracing::info!("server workspace tree pruned to nothing; keeping the flat layout");
            return false;
        }
        // The reader filled the strip in `SessionList` order, which is the
        // server's storage order and not the user's. The tree carries the order
        // this window last reported — every tab of every region, left to right —
        // so it, not the list, is what the strip is restored to. Ordering by the
        // *placed* sessions alone (what this did) left every background tab
        // wherever the list happened to put it.
        let order = pane_shell::wire_tree_tab_order(&tree);
        if let Ok(mut tabs) = self.shared.tabs.lock() {
            tabs.order_by(&order);
        }
        tracing::info!(
            panes = visible.len(),
            regions = self.shell.region_count(cx),
            "rebuilt workspace splits from the server's tree"
        );
        // The adopted layout decides which tab is active, not whichever session
        // the reader happened to attach first: each region came back showing the
        // tab its `active_tab_index` named, and the strip has to agree or the
        // titlebar would highlight a tab that is not the one on screen.
        if let Some(session_id) = self.shell.focused_session(cx)
            && let Ok(mut tabs) = self.shared.tabs.lock()
        {
            tabs.show(session_id);
        }
        // The pane holding the window's attached session keeps focus, so the
        // reader's own reattach and this rebuild agree on the active pane.
        let active = self.shared.active_session.lock().ok().and_then(|guard| *guard);
        if let Some(session_id) = active
            && let Some((workspace_id, pane)) = self.shell.pane_for_session(session_id, cx)
        {
            self.shell.focus_pane(workspace_id, pane, cx);
        }
        // Attach every visible pane's session; the focused pane's goes last so
        // it ends up as the active (typed-into) session.
        let focused = self.shell.focused_session(cx);
        for session_id in visible.iter().filter(|id| Some(**id) != focused) {
            self.attach(*session_id, cx);
        }
        if let Some(session_id) = focused {
            self.attach(session_id, cx);
        }
        // Now that every pane session is attached, any publish resize and
        // snapshot are authorised. `stream_session` already cached each exact
        // placement, so unchanged panes are skipped.
        self.publish_pane_sizes(cx);
        self.sync_tabs(cx);
        // Report the (possibly pruned) adopted layout back so the server's
        // tree matches what the window actually shows, and persist it as this
        // window's fresh cold-restart snapshot.
        self.report_workspace_tree(cx);
        self.restore.mark_layout_dirty();
        cx.notify();
        true
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
        // The server's persisted split tree is folded in first: on a fresh
        // (re)connect it rebuilds regions and splits that everything below —
        // the root-region adoption, the metadata drain, and the active-session
        // placement — then operates on instead of flattening.
        let adopted = self.adopt_server_topology(cx);
        // The root region must adopt the active server id before metadata is
        // drained: `SessionCreated` and its following `WorkspaceInfo` can both
        // land before one frame, and applying the latter against the original
        // client-local id would discard its project root as unclaimed.
        let mut changed = false;
        // The *server's* id for the first region, not the shell's: the shell is
        // still on its client-minted id at this point, and this is the frame
        // that replaces it.
        let seed = self
            .shared
            .tabs
            .lock()
            .ok()
            .and_then(|tabs| tabs.regions().first().map(|region| region.workspace_id));
        if let Some(workspace_id) = seed {
            changed |= self.shell.adopt_server_workspace(workspace_id, cx);
        }
        // Metadata still lands before pane/session adoption. A `WorkspaceInfo`
        // answering `CreateWorkspace` therefore re-keys the split region before
        // its session is adopted below and moved into that server workspace.
        changed |= self.adopt_workspace_info(cx);
        let (live, workspaces_with_tabs) = self.shared.tabs.lock().map_or_else(
            |_| (HashSet::new(), HashSet::new()),
            |tabs| {
                (
                    tabs.entries().map(|tab| tab.session_id).collect::<HashSet<SessionId>>(),
                    tabs.regions()
                        .iter()
                        .map(|region| region.workspace_id)
                        .collect::<HashSet<WorkspaceId>>(),
                )
            },
        );
        // The retain pass is skipped on the frame an adoption rebuilt the
        // regions: the reader parks the tree before it rebuilds the strip, so
        // this frame's strip can predate the adopted layout, and judging fresh
        // regions against a stale strip is how workspaces get closed on the
        // server they were just restored from. Next frame both are current.
        if !live.is_empty() && !adopted {
            self.retire_scrollbars(&live);
            let retired = self.shell.retain_sessions(&live, &workspaces_with_tabs, cx);
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
            if let Some(pane) = self.shell.take_pending(cx) {
                self.adopt_session(pane, session_id);
                self.follow_session_to_region(pane, session_id, cx);
                changed = true;
            } else if let Some((_, pane)) = self.tab_adoption_pane(session_id, cx) {
                self.adopt_session(pane, session_id);
                self.follow_session_to_region(pane, session_id, cx);
                changed = true;
            }
        }
        changed |= self.fill_empty_region_panes(cx);
        changed |= self.fill_pending_panes(cx);
        if changed {
            self.publish_pane_sizes(cx);
            self.report_workspace_tree(cx);
        }
    }

    /// Hand each surviving empty pane an unshown tab of its own workspace.
    ///
    /// A region whose only pane's session exited keeps that pane when its
    /// workspace still has tabs ([`PaneShell::retain_sessions`]); this is the
    /// pass that fills it back in, strictly workspace-scoped so a refill can
    /// never move a tab between regions. Streaming (attach + subscribe) is
    /// done without touching the active session, because the refilled region
    /// may not be the one the user is typing in.
    fn fill_empty_region_panes(&mut self, cx: &mut Context<Self>) -> bool {
        let empties = self.shell.empty_unpending_panes(cx);
        if empties.is_empty() {
            return false;
        }
        let entries: Vec<(WorkspaceId, SessionId)> = self.shared.tabs.lock().map_or_else(
            |_| Vec::new(),
            |tabs| tabs.entries().map(|tab| (tab.workspace_id, tab.session_id)).collect(),
        );
        let mut changed = false;
        for (workspace_id, pane) in empties {
            let shown = self.shell.shown_sessions();
            let refill = entries
                .iter()
                .find(|(tab_ws, session)| *tab_ws == workspace_id && !shown.contains(session))
                .map(|(_, session)| *session);
            let Some(session_id) = refill else { continue };
            self.adopt_session(pane, session_id);
            self.stream_session(session_id, cx);
            tracing::info!(%session_id, %workspace_id, "refilled an emptied pane from its workspace's tabs");
            changed = true;
        }
        changed
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
                tabs.entries().map(|tab| tab.session_id).find(|id| !shown.contains(id))
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
        self.seed_restored_prompts(pane, session_id);
        tracing::info!(%session_id, pane = pane.raw(), "pane adopted a session");
    }

    /// Hand a replayed pane's persisted prompt history to the session it has
    /// just adopted.
    ///
    /// This is the wire between the two halves of prompt-bar persistence: the
    /// snapshot files prompts under a pane, the bar reads them out of
    /// [`AiChrome`] under a session, and only an adoption knows both. Without
    /// it a correctly saved snapshot is read back, written straight out again,
    /// and never rendered — the bar does not exist at `prompt_count == 0`.
    /// Every adoption path routes through [`Self::adopt_session`], so a
    /// restored pane is seeded whether the replay's own fill or the ordinary
    /// active-session path gets to it first.
    fn seed_restored_prompts(&mut self, pane: PaneId, session_id: SessionId) {
        let Some(restored) = self.restore.restored_prompts.remove(&pane) else { return };
        let Ok(mut ai) = self.shared.ai.lock() else {
            tracing::warn!(%session_id, "AI chrome mutex poisoned; restored prompt bar stays empty");
            return;
        };
        ai.restore_prompts(session_id, restored.prompts, restored.conversation_id);
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
    fn pane_content(&self, session_id: SessionId) -> Option<Arc<Content>> {
        self.pane_frame(session_id).map(|frame| {
            debug_assert!(
                frame.image_scene.definitions.len()
                    <= usize::try_from(ImageLimits::V1.max_images_per_session)
                        .unwrap_or(usize::MAX)
            );
            Arc::clone(&frame.content)
        })
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

    fn terminal_images_paint(
        &self,
        session_id: Option<SessionId>,
        active_sessions: Rc<HashSet<SessionId>>,
    ) -> TerminalImagesPaint {
        let scene = session_id.and_then(|id| self.pane_frame(id)).map_or_else(
            || Arc::new(CommittedImageScene::default()),
            |frame| Arc::clone(&frame.image_scene),
        );
        TerminalImagesPaint {
            session_id,
            scene,
            cache: Rc::clone(&self.image_cache),
            active_sessions,
        }
    }

    /// Mint the per-pane state the render closure below only borrows.
    ///
    /// Both maps outlive the elements that read them — the scrollbar fade is a
    /// wall-clock animation across frames, and a bounds sink is written during
    /// paint and read by the *next* frame's pointer events — so they live here
    /// and are minted before the closure takes `self` immutably.
    fn prepare_pane_surfaces(
        &mut self,
        placements: &[pane_shell::PanePlacement],
    ) -> Rc<HashSet<SessionId>> {
        let sessions = placements.iter().filter_map(|placement| placement.session_id);
        for session_id in sessions.clone() {
            self.scrollbars.panes.entry(session_id).or_default();
            self.pane_bounds.entry(session_id).or_default();
        }
        Rc::new(sessions.collect())
    }

    /// Lower the pane layout onto absolutely positioned grid elements.
    ///
    /// Positions are fractions of the grid area, so the split ratios the pure
    /// tree computed survive any window size without the view having to measure
    /// device pixels. The focus ring is only drawn once a window actually has
    /// more than one pane, so an unsplit window paints exactly as before.
    ///
    /// Find matches and the split-scroll pin belong to the focused pane: the
    /// overlay searched the pane the query was typed against, and the pin
    /// follows that pane's viewport. `focused` is therefore the snapshot
    /// [`Self::sync_split_scroll`] already pinned, and every other pane paints
    /// its own untouched grid. The recorded grid bounds are *not* in that set —
    /// every pane records its own, because the pointer gestures that read them
    /// pick their pane by position rather than by focus.
    fn render_panes(
        &mut self,
        focused: FocusedPanePaint,
        cx: &Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let FocusedPanePaint { content: focused, ime, cursor, link } = focused;
        let viewport = self.pane_viewport();
        if viewport.width <= 0.0 || viewport.height <= 0.0 {
            return Vec::new();
        }
        let placements = self.shell.placements(viewport, cx);
        let active_sessions = self.prepare_pane_surfaces(&placements);
        let workspace_ai_borders = self.workspace_ai_borders(&placements);
        // Mint any missing scrollbar state before the render closure below
        // borrows `self` immutably. The state has to outlive the element (the
        // fade is a wall-clock animation across frames), so it lives here and
        // the element only borrows a handle to it.
        let split = placements.len() > 1;
        let selection_spans = self.selection_spans();
        let mut ime = Some(ime);
        let mut link_rows = link.map(|link| link.rows);
        let opacity = self.opacity;
        // Identical for every pane, so it is built once and cloned in: the
        // clone is an `Rgba`, an `Arc` bump, and an `f32`.
        let colors = GridColors {
            background: surface(self.terminal_colors.background, opacity),
            cells: Arc::clone(&self.terminal_colors.cells),
            opacity,
        };
        let idle_border = surface(self.chrome.divider, opacity);
        let chrome_bg = scribe_client::tab_bar::srgba(self.chrome.tab_bar_bg);
        let mut focused = Some(focused);
        placements
            .into_iter()
            .map(|placement| {
                let session_id = placement.session_id;
                let image_paint =
                    self.terminal_images_paint(session_id, Rc::clone(&active_sessions));
                let content = if placement.focused {
                    focused.take().unwrap_or_default()
                } else {
                    placement.session_id.and_then(|s| self.pane_content(s)).unwrap_or_default()
                };
                let mut pane = div()
                    .absolute()
                    .left(relative(placement.rect.x / viewport.width))
                    .top(relative(placement.rect.y / viewport.height))
                    .w(relative(placement.rect.width / viewport.width))
                    .h(relative(placement.rect.height / viewport.height))
                    .overflow_hidden();
                if split {
                    let border = pane_border(&placement, idle_border, chrome_bg, opacity);
                    pane = pane.border_1().border_color(border);
                }
                let ai_border = workspace_ai_borders.get(&placement.workspace_id).copied();
                // These three belong to the focused pane alone, for one reason:
                // each is driven by something this window resolves against
                // exactly one pane — the overlay's matches, the mouse
                // selection, and the Ctrl-hovered link rule. Taken rather than
                // cloned, the way the snapshot above is.
                let (highlights, selection, underline) = if placement.focused {
                    (
                        self.find_highlights(&content, cx),
                        selection_spans.clone(),
                        link_rows.take().unwrap_or_default(),
                    )
                } else {
                    (Vec::new(), Vec::new(), Vec::new())
                };
                let bounds = self.pane_bounds_sink(session_id);
                if !underline.is_empty() {
                    pane = pane.cursor_pointer();
                }
                // The IME handler is a window-level singleton, so `take` gives
                // it to the focused pane and to nothing else even if the layout
                // ever reported two focused placements. The scrollbar is the
                // opposite: per-pane, so every pane showing a session gets one.
                let element = TerminalElement::new(
                    content,
                    self.font.clone(),
                    colors.clone(),
                    self.highlight_colors,
                    bounds,
                )
                .with_highlights(highlights)
                .with_selection(selection)
                .with_link_underline(underline)
                .with_terminal_images(image_paint)
                .with_cursor(placement.focused.then_some(cursor))
                .with_scrollbar(placement.session_id.and_then(|s| self.scrollbar_paint(s)))
                .with_ime(placement.focused.then(|| ime.take()).flatten());
                let pane = self.compose_pane_content(pane, element.paint(), &placement, cx);
                let mut pane = Self::attach_wheel(pane, &placement, cx);
                if let Some(color) = ai_border {
                    pane = pane.children(ai_pane_border(placement.rect, color));
                }
                pane.into_any_element()
            })
            .collect()
    }

    /// Hang the wheel off one pane, so GPUI's own hit test decides which
    /// terminal a scroll belongs to.
    ///
    /// Hovering a pane scrolls it whether or not it holds focus, and whether or
    /// not this window is the active one: the wheel is a pointer gesture, so it
    /// must act on what the pointer is over without taking the keyboard's pane
    /// away from it. The grid rect travels with the listener because a mouse
    /// report carries a cell, which the handler reads out of that pane's own
    /// bounds sink. A pane still waiting on `SessionCreated` gets no listener
    /// at all — there is nothing to scroll yet.
    fn attach_wheel(pane: gpui::Div, placement: &PanePlacement, cx: &Context<Self>) -> gpui::Div {
        let Some(session) = placement.session_id else { return pane };
        pane.on_scroll_wheel(cx.listener(move |view, event: &ScrollWheelEvent, _window, ctx| {
            view.scroll_pane(session, event, ctx);
        }))
    }

    /// Stack a pane's grid and its optional prompt strip inside the pane div.
    ///
    /// The prompt strip is pane chrome: it renders inside the pane that runs
    /// the AI session, above or below that pane's grid per
    /// `terminal.prompt_bar_position`, never spanning its neighbours.
    fn compose_pane_content(
        &self,
        pane: gpui::Div,
        grid: impl IntoElement,
        placement: &pane_shell::PanePlacement,
        cx: &Context<Self>,
    ) -> gpui::Div {
        let colors = self.prompt_colors.with_opacity(self.opacity);
        let metrics = self.prompt_bar_metrics();
        let strip = placement
            .session_id
            .and_then(|s| self.prompt_model_for(s).map(|model| (s, model)))
            .map(|(session_id, model)| {
                let actions = self.prompt_bar_actions(session_id, placement.rect.width, cx);
                prompt_bar::render(&model, &colors, metrics, actions).into_any_element()
            });
        let grid_slot = div().flex_1().min_h(px(0.0)).overflow_hidden().child(grid);
        let pane = pane.flex().flex_col();
        if self.prompt_bar.position == PromptBarPosition::Top {
            pane.children(strip).child(grid_slot)
        } else {
            pane.child(grid_slot).children(strip)
        }
    }

    /// Paint each live workspace and pane divider above the grids it separates.
    fn render_dividers(&self, cx: &App) -> Vec<gpui::AnyElement> {
        let viewport = self.pane_viewport();
        let workspace_dividers = self.shell.workspace_dividers(viewport, cx);
        let cursor_bands = workspace_dividers
            .iter()
            .map(|divider| {
                let rect = workspace_layout::workspace_divider_hit_rect(divider);
                div()
                    .absolute()
                    .left(px(rect.x))
                    .top(px(rect.y))
                    .w(px(rect.width))
                    .h(px(rect.height))
                    .cursor(Self::workspace_divider_cursor(divider.direction))
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        workspace_dividers
            .into_iter()
            .map(|divider| divider.rect)
            .chain(self.shell.dividers(viewport, cx).into_iter().map(|divider| divider.rect))
            .map(|rect| {
                div()
                    .absolute()
                    .left(px(rect.x))
                    .top(px(rect.y))
                    .w(px(rect.width))
                    .h(px(rect.height))
                    .bg(surface(self.chrome.divider, self.opacity))
                    .into_any_element()
            })
            .chain(cursor_bands)
            .collect()
    }

    /// Use the native pointer that matches a workspace split's resize axis.
    const fn workspace_divider_cursor(direction: SplitDirection) -> gpui::CursorStyle {
        match direction {
            SplitDirection::Horizontal => gpui::CursorStyle::ResizeLeftRight,
            SplitDirection::Vertical => gpui::CursorStyle::ResizeUpDown,
        }
    }

    /// Paint each lower region's tab bar over the strip it reserved.
    ///
    /// Regions on the window's top row keep their tabs in the titlebar; a
    /// region stacked below gets the legacy client's in-region bar here, at
    /// the top of its rect, above the panes [`PaneShell::placements`] already
    /// shrank to make room.
    fn render_region_tab_bars(&self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let bar_rects = self.shell.region_bar_rects(self.pane_viewport(), cx);
        if bar_rects.is_empty() {
            return Vec::new();
        }
        let colors = TabBarColors::from_chrome(&self.chrome, self.opacity);
        bar_rects
            .into_iter()
            .map(|(workspace_id, rect)| self.render_region_bar(workspace_id, rect, &colors, cx))
            .collect()
    }

    /// One lower region's bar: badge pill, then its tabs, over a hairline in
    /// the region's tab tone — the same shape the titlebar draws for a
    /// top-row region's group.
    /// Advance a region bar's tab drag to `cursor_x`, swapping slots as the
    /// dragged tab's centre crosses into a neighbour's.
    ///
    /// The bar's slots and the strip's are the same positions now that a region
    /// owns its own tab list, so the swap applies directly and a drag cannot
    /// leave its region by construction.
    fn update_region_drag(&mut self, cursor_x: f32, cx: &mut Context<Self>) {
        let Some(mut drag) = self.region_chrome.drag else { return };
        let Some(bar) =
            self.region_chrome.bars.iter().find(|bar| bar.workspace_id == drag.workspace_id)
        else {
            return;
        };
        let count = bar.tabs.len();
        if count == 0 {
            return;
        }
        // Reuse the titlebar's edge walker rather than round pointer travel
        // here: it clamps and avoids a float-to-index cast. Anchoring its origin
        // half a tab left of the press makes the walk resolve to
        // `origin + round(travel / TAB_WIDTH)`, so the swap boundary still sits
        // at half a tab of overlap wherever inside the tab the grab landed.
        let anchor = drag.press_x - px_units(drag.origin) * TAB_WIDTH - TAB_WIDTH / 2.0;
        let target = reorder_target_index(cursor_x, anchor, TAB_WIDTH, count, drag.current);
        if target == drag.current {
            return;
        }
        let Ok(mut tabs) = self.shared.tabs.lock() else { return };
        if !tabs.reorder(drag.workspace_id, drag.current, target) {
            return;
        }
        drop(tabs);
        drag.current = target;
        drag.reordered = true;
        self.region_chrome.drag = Some(drag);
        // Same reason the titlebar reports on every swap: the reported tree is
        // the only place tab order is durable.
        self.report_workspace_tree(cx);
        cx.notify();
    }

    /// Clear the region drag on release, keeping every swap it committed.
    fn end_region_drag(&mut self) {
        self.region_chrome.drag = None;
    }

    #[allow(clippy::too_many_lines, reason = "one region bar owns its complete interaction tree")]
    #[allow(
        clippy::excessive_nesting,
        reason = "GPUI declarative element listeners nest by design"
    )]
    fn render_region_bar(
        &self,
        workspace_id: WorkspaceId,
        rect: Rect,
        colors: &TabBarColors,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let bar = self.region_chrome.bars.iter().find(|bar| bar.workspace_id == workspace_id);
        let accent =
            bar.map_or_else(|| self.shell.workspace_accent(workspace_id, cx), |bar| bar.accent);
        let tone = accent_tab_tone(opaque_slot(accent), colors.bg);
        let mut row = div()
            .absolute()
            .left(px(rect.x))
            .top(px(rect.y))
            .w(px(rect.width))
            .h(px(rect.height))
            .flex()
            .flex_row()
            .items_center()
            .overflow_hidden()
            .bg(colors.bg)
            .border_b_1()
            .border_color(tone)
            // Every move while a region tab drag is active, so the drag keeps
            // tracking the cursor after it leaves this bar's band.
            .on_drag_move(cx.listener(
                |view, event: &DragMoveEvent<RegionTabDrag>, _win, ctx| {
                    view.update_region_drag(f32::from(event.event.position.x), ctx);
                },
            ))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|view, _: &MouseUpEvent, _win, _ctx| view.end_region_drag()),
            )
            // A release outside the bar still ends the drag; the swaps already
            // committed stay, and no stale state can pin the tab off its slot.
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|view, _: &MouseUpEvent, _win, _ctx| view.end_region_drag()),
            );
        let Some(bar) = bar else {
            // A region whose seed session is still in flight: bare bar, so the
            // reserved strip never flashes terminal background.
            return row.into_any_element();
        };
        // The workspace pill opening this bar: clicking it selects the bar's
        // first tab, which focuses the region. A badge with no tabs yet has
        // nothing to select, so it waits for the seed session.
        let first_session = bar.tabs.first().map(|(session_id, _)| *session_id);
        if let Some((badge, first)) = bar.badge.as_ref().zip(first_session) {
            let label = div()
                .id(ElementId::from(format!("region-badge-{workspace_id}")))
                .flex()
                .items_center()
                .px_2()
                .h_full()
                .text_color(colors.active_text)
                .text_xs()
                .cursor_pointer()
                .on_mouse_down(MouseButton::Left, |_, _win, ctx| ctx.stop_propagation())
                .on_click(cx.listener(move |view, _, _window, ctx| {
                    view.select_session_tab(first, ctx);
                }))
                .child(badge.label.clone());
            let mut pill = div().flex().flex_none().items_center().h_full().bg(tone);
            if badge.beads {
                pill = pill.child(
                    div()
                        .id(ElementId::from(format!("region-beads-{workspace_id}")))
                        .role(gpui::Role::Button)
                        .aria_label(format!("Open {} Beads board", badge.label))
                        .flex()
                        .items_center()
                        .justify_center()
                        .w(px(26.0))
                        .h_full()
                        .text_color(colors.accent)
                        .cursor_pointer()
                        .hover(|style| style.bg(colors.gradient_top))
                        .on_hover(cx.listener(move |view, hovered: &bool, _window, ctx| {
                            let refresh =
                                view.shared.beads_boards.lock().is_ok_and(|mut boards| {
                                    boards.hover(workspace_id, HoverSource::Bead, *hovered);
                                    boards.needs_refresh(workspace_id, BEADS_HOVER_REFRESH_AGE)
                                });
                            if *hovered && refresh {
                                request_beads_board_or_log(
                                    &view.sink,
                                    workspace_id,
                                    "region hover refresh",
                                );
                            }
                            ctx.notify();
                        }))
                        .on_mouse_down(MouseButton::Left, |_, _window, ctx| {
                            ctx.stop_propagation();
                        })
                        .on_click(cx.listener(move |view, _, _window, ctx| {
                            let refresh =
                                view.shared.beads_boards.lock().is_ok_and(|mut boards| {
                                    boards.toggle_pin(workspace_id);
                                    boards.needs_refresh(workspace_id, BEADS_HOVER_REFRESH_AGE)
                                });
                            if refresh {
                                request_beads_board_or_log(
                                    &view.sink,
                                    workspace_id,
                                    "region pin refresh",
                                );
                            }
                            ctx.notify();
                        }))
                        .child(beads_graph_icon(colors.accent)),
                );
            }
            let pill = pill.child(label);
            row = row.child(pill);
        }
        row = row.children(bar.tabs.iter().enumerate().map(|(index, (session_id, tab))| {
            let slot = RegionTabSlot { workspace_id, index };
            Self::render_region_bar_tab(*session_id, tab, slot, colors, cx)
        }));
        row.into_any_element()
    }

    /// One tab in a lower region's bar: the titlebar tab's look (flash blend,
    /// Press, drag and click wiring for one region bar tab.
    ///
    /// Split out of the render so the element builder stays readable: this is
    /// the only part of a region tab that is behaviour rather than paint.
    fn region_tab_interactions(
        tab: gpui::Stateful<gpui::Div>,
        session_id: SessionId,
        slot: RegionTabSlot,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let RegionTabSlot { workspace_id, index } = slot;
        tab
            // Arms the drag and stops propagation, so the bar's own press never
            // reaches the titlebar's window-move arm behind it.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |view, event: &MouseDownEvent, _win, ctx| {
                    view.region_chrome.drag = Some(RegionTabDragState {
                        workspace_id,
                        origin: index,
                        current: index,
                        press_x: f32::from(event.position.x),
                        reordered: false,
                    });
                    ctx.stop_propagation();
                }),
            )
            // Registers the drag once the pressed pointer passes GPUI's
            // threshold; from then on the bar's `on_drag_move` sees every move.
            .on_drag(RegionTabDrag, |_, _, _, cx| cx.new(|_| RegionTabDragGhost))
            .on_click(cx.listener(move |view, _, _window, ctx| {
                // A drag that reordered is not a click: selecting here would
                // fight the reorder the release just committed.
                if view.region_chrome.drag.is_some_and(|drag| drag.reordered) {
                    return;
                }
                view.select_session_tab(session_id, ctx);
            }))
    }

    /// AI dot, context suffix, active underline) minus its keyboard chrome.
    ///
    /// Drag-reorder is not part of that subtraction. A region bar holds every
    /// tab of every region below the first, so leaving it out made those tabs
    /// the only ones in the window a user could not reorder — the titlebar's
    /// drag reached the top region alone.
    fn render_region_bar_tab(
        session_id: SessionId,
        tab: &TabData,
        slot: RegionTabSlot,
        colors: &TabBarColors,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let base_bg = if tab.is_active { colors.active_bg } else { colors.bg };
        let bg = flash_blend(base_bg, colors.accent, tab.tab_flash);
        let fg = if tab.is_active { colors.active_text } else { colors.text };
        let hover_bg = gpui::Rgba {
            r: (colors.bg.r + 0.04).min(1.0),
            g: (colors.bg.g + 0.04).min(1.0),
            b: (colors.bg.b + 0.04).min(1.0),
            a: colors.bg.a,
        };
        let suffix_len = tab.context_suffix.as_ref().map_or(0, |s| s.text.chars().count());
        let (display, _truncated) =
            tab_display_title(&tab.title, title_columns(suffix_len, tab.ai_indicator.is_some()));
        let ai_dot = tab
            .ai_indicator
            .map(|color| div().size(px(6.0)).rounded_full().bg(color).mr_2().into_any_element());
        let suffix = tab.context_suffix.as_ref().map(|suffix| {
            div().text_color(suffix.color).child(suffix.text.clone()).into_any_element()
        });
        let underline = tab.is_active.then(|| {
            let tone =
                tab.group_accent.map_or(colors.accent, |accent| accent_tab_tone(accent, colors.bg));
            div().absolute().bottom_0().left_0().right_0().h(px(2.0)).bg(tone).into_any_element()
        });
        let close = tab.is_active.then(|| {
            div()
                .id(ElementId::from(format!("region-tab-close-{session_id}")))
                .flex_none()
                .ml_1()
                .px_0p5()
                .text_color(fg)
                .cursor_pointer()
                .child("\u{00D7}")
                .on_mouse_down(MouseButton::Left, |_, _win, ctx| ctx.stop_propagation())
                .on_click(cx.listener(move |view, _, _window, _ctx| {
                    view.close_session(session_id, "tab close dropped");
                }))
                .into_any_element()
        });
        div()
            .id(ElementId::from(format!("region-tab-{session_id}")))
            .relative()
            .flex()
            .items_center()
            .flex_grow_0()
            .flex_shrink_1()
            .flex_basis(px(TAB_WIDTH))
            .min_w(px(TAB_MIN_WIDTH))
            .overflow_hidden()
            .h_full()
            .px_2()
            .bg(bg)
            .text_color(fg)
            .text_xs()
            .border_r_1()
            .border_color(colors.separator)
            .cursor_pointer()
            .when(!tab.is_active, |this| this.hover(move |s| s.bg(hover_bg)))
            .map(|tab| Self::region_tab_interactions(tab, session_id, slot, cx))
            .children(ai_dot)
            // `truncate`, as in the titlebar: without it a title that outgrows
            // the flexed slot once the AI dot appears wraps onto a second line
            // inside the fixed-height tab and the visible text rides up.
            .child(div().flex_1().truncate().child(display))
            .children(suffix)
            .children(close)
            .children(underline)
            .into_any_element()
    }

    /// Focus the tab attached to `session_id`, wherever it sits in the strip.
    ///
    /// Logged because this is the only pointer entry point the in-region bars
    /// have, and the switch it triggers is otherwise indistinguishable on the
    /// wire from a titlebar click — the log is what lets a scripted E2E
    /// attribute the attach to the bar.
    fn select_session_tab(&mut self, session_id: SessionId, cx: &mut Context<Self>) {
        tracing::info!(%session_id, "region bar selected a tab");
        self.activate_session_tab(session_id, cx);
    }

    /// Activate `session_id` from a pointer: show it in its own region and move
    /// the window's focus there.
    ///
    /// A click on an unfocused region's bar means "focus this region, on this
    /// tab", and that region is usually *already showing* the clicked tab — so
    /// the strip reports no change and the no-change contract that keeps
    /// keyboard repeat from re-attaching would swallow exactly that click. The
    /// window's own focus is the second half of the condition, which is what
    /// makes a click across regions act while a click on the tab the user is
    /// already in stays a no-op.
    fn activate_session_tab(&mut self, session_id: SessionId, cx: &mut Context<Self>) {
        let moved = self.shared.tabs.lock().is_ok_and(|mut tabs| tabs.show(session_id).is_some());
        if !moved && self.shell.focused_session(cx) == Some(session_id) {
            return;
        }
        self.switch_tab(move |_| Some(session_id), cx);
    }

    /// Translate a window pointer position into the grid band's local space.
    fn grid_local_position(&self, position: Point<Pixels>) -> Option<(f32, f32)> {
        let bounds = self.grid_area.get()?;
        Some((
            f32::from(position.x) - f32::from(bounds.origin.x),
            f32::from(position.y) - f32::from(bounds.origin.y),
        ))
    }

    /// Start a resize when the pointer lands on an open board's bottom bar.
    ///
    /// Resolved here rather than by a listener inside the board so the press is
    /// consumed the way a divider's is: the pane under an unpinned board never
    /// sees it, and a mouse-reporting application below is not handed the press
    /// that started a chrome gesture.
    fn press_board_edge(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) -> bool {
        if self.visible_beads_boards.is_empty() {
            return false;
        }
        let Some((x, y)) = self.grid_local_position(position) else { return false };
        let viewport = self.pane_viewport();
        let Ok(mut boards) = self.shared.beads_boards.lock() else { return false };
        let grabbed = self.visible_beads_boards.iter().find(|(workspace_id, _)| {
            self.shell
                .board_rect(*workspace_id, boards.height(*workspace_id), viewport, cx)
                .is_some_and(|rect| {
                    x >= rect.x
                        && x < rect.x + rect.width
                        && (y - (rect.y + rect.height)).abs() <= BEADS_BOARD_GRIP
                })
        });
        let Some((workspace_id, _)) = grabbed else { return false };
        boards.start_resize(*workspace_id, y);
        true
    }

    /// Apply an in-flight board resize.
    ///
    /// The new height is published by the ordinary paint that follows: the
    /// strip a pinned board reserves is re-read from this state every frame,
    /// and the pane sizes are republished from the rects that leaves.
    fn drag_board(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) -> bool {
        let Ok(mut boards) = self.shared.beads_boards.lock() else { return false };
        let Some(workspace_id) = boards.resizing() else { return false };
        let Some((_, y)) = self.grid_local_position(position) else { return true };
        // A board may take its region up to the last few lines of terminal.
        // Growing past that is what the user asked for right up until the
        // terminal disappears, and there is no gesture to get it back.
        let viewport = self.pane_viewport();
        let max = self
            .shell
            .board_rect(workspace_id, f32::MAX, viewport, cx)
            .map_or(0.0, |content| content.height - self.font.line_height * 3.0);
        if boards.resize_to(y, max) {
            drop(boards);
            cx.notify();
        }
        true
    }

    /// Start a drag when the pointer lands in a divider's hit band.
    fn press_divider(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) -> bool {
        let Some((x, y)) = self.grid_local_position(position) else { return false };
        let viewport = self.pane_viewport();
        let workspace_dividers = self.shell.workspace_dividers(viewport, cx);
        if let Some(divider) =
            workspace_layout::hit_test_workspace_divider(&workspace_dividers, x, y)
        {
            self.pointer.workspace_divider_drag =
                Some(workspace_layout::start_workspace_drag(divider));
            return true;
        }
        let dividers = self.shell.dividers(viewport, cx);
        let Some(divider) = divider::hit_test_divider(&dividers, x, y) else {
            return false;
        };
        self.pointer.divider_drag = Some(divider::start_drag(divider, viewport));
        true
    }

    /// Apply an in-flight divider drag and republish both panes' geometry.
    fn drag_divider(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) -> bool {
        if let Some(drag) = self.pointer.workspace_divider_drag {
            let Some((x, y)) = self.grid_local_position(position) else { return true };
            let mouse_pos = match drag.direction {
                SplitDirection::Horizontal => x,
                SplitDirection::Vertical => y,
            };
            if self.shell.set_workspace_ratio(
                drag.first_workspace,
                drag.second_workspace,
                workspace_layout::workspace_drag_ratio(&drag, mouse_pos),
                cx,
            ) {
                self.after_layout_change(cx);
            }
            return true;
        }
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
    /// Consumers in priority order. A divider press is a resize and owns the
    /// boundary outright. The overlay scrollbar comes next because it is chrome
    /// painted over the cells: a click on the thumb was never meant for the
    /// application below it, which is the order the winit client resolved its
    /// chrome in too — and because it is a scroll gesture, it precedes the
    /// focusing click rather than waiting behind one. Focusing an unfocused
    /// pane follows, then the Ctrl-click link, then a mouse-tracking
    /// application; only when all of them decline does the press mean
    /// selection.
    fn press_grid(&mut self, event: &MouseDownEvent, cx: &mut Context<Self>) {
        if self.press_board_edge(event.position, cx)
            || self.press_divider(event.position, cx)
            || self.press_scrollbar(event.position, cx)
            || self.press_focuses_pane(event.position, cx)
            || self.press_opens_link(event, cx)
            || self.forward_mouse_press(event)
        {
            return;
        }
        self.click_grid(event.position, cx);
    }

    /// Focus the unfocused pane under a press, so clicking into any visible
    /// terminal focuses it directly instead of requiring its tab first.
    ///
    /// Runs after the divider hit test (a boundary press must stay a resize)
    /// and consumes the gesture: the first click into a pane focuses it, and
    /// the next press interacts with its content, matching common tiling UX.
    fn press_focuses_pane(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) -> bool {
        let Some((x, y)) = self.grid_local_position(position) else { return false };
        let viewport = self.pane_viewport();
        let pressed = self.shell.placements(viewport, cx).into_iter().find(|placement| {
            x >= placement.rect.x
                && x < placement.rect.x + placement.rect.width
                && y >= placement.rect.y
                && y < placement.rect.y + placement.rect.height
        });
        let Some(placement) = pressed else { return false };
        if placement.focused {
            return false;
        }
        // Logged under the same message the focus chords use: a focus move is
        // a focus move whichever gesture asked for it, and one grep has to find
        // them all.
        tracing::info!(pane = placement.pane_id.raw(), "focused pane moved");
        let Some(session_id) = placement.session_id else {
            self.shell.focus_pane(placement.workspace_id, placement.pane_id, cx);
            cx.notify();
            return true;
        };
        // Route through the pointer activation path so the strip's shown tab,
        // the attach, and the focus report all follow the pane focus.
        self.activate_session_tab(session_id, cx);
        cx.notify();
        true
    }

    /// Resolve pointer motion over the grid band.
    ///
    /// An in-flight thumb drag owns the pointer outright. Otherwise hover is
    /// tracked even while an application owns the pointer — the hover widen is
    /// what makes the thumb grabbable, and a press on it would have been
    /// claimed by the scrollbar anyway — before the motion falls through to
    /// mouse reporting and then to extending a selection.
    fn move_over_grid(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        if self.drag_board(event.position, cx)
            || self.drag_divider(event.position, cx)
            || self.drag_scrollbar(event.position, cx)
        {
            return;
        }
        self.update_scrollbar_hover(event.position, cx);
        if let Some(session_id) = hover_focus_target(
            self.config.config().config.terminal.focus.focus_follows_mouse,
            event.pressed_button,
            self.pane_at(event.position),
            self.focused_session(),
        ) {
            tracing::info!(%session_id, "focused pane moved");
            self.activate_session_tab(session_id, cx);
            cx.notify();
            return;
        }
        // The link rule follows the pointer, and it is read off the window at
        // paint time rather than tracked here — so the move only has to ask for
        // the repaint that will re-read it. Gated on Ctrl so an ordinary mouse
        // move over the grid still costs nothing.
        if event.modifiers.control {
            cx.notify();
        }
        if self.forward_mouse_motion(event) {
            return;
        }
        self.extend_selection(event.position, cx);
    }

    /// Resolve a left release over the grid band, ending whichever gesture the
    /// matching press started.
    fn release_over_grid(&mut self, event: &MouseUpEvent, cx: &mut Context<Self>) {
        // The press already opened a link, so this release belongs to a gesture
        // that is over. Forwarding it would hand a mouse-tracking application a
        // release with no press.
        if self.pointer.drag == GridDrag::Link {
            self.pointer.drag = GridDrag::Idle;
            return;
        }
        if self.release_board(cx)
            || self.pointer.workspace_divider_drag.take().is_some()
            || self.pointer.divider_drag.take().is_some()
            || self.release_scrollbar(cx)
            || self.forward_mouse_release(event)
        {
            return;
        }
        self.finish_selection(cx);
    }

    /// Let go of either board gesture, reporting whether one was held.
    ///
    /// The board is left at whatever height the drag reached; a hovered one
    /// then closes on the grace period the drag was holding open.
    fn release_board(&mut self, cx: &mut Context<Self>) -> bool {
        let (released, drag) =
            self.shared.beads_boards.lock().map_or((false, None), |mut boards| {
                let card = boards.blocks_pty_mouse();
                let drag = boards.take_card_drag();
                let resize = boards.end_resize();
                (card || resize, drag)
            });
        if let Some(drag) = drag {
            let accepted = self
                .shared
                .beads_panels
                .lock()
                .is_ok_and(|mut panels| panels.queue_card_drop(&drag));
            if accepted && let Ok(mut boards) = self.shared.beads_boards.lock() {
                boards.apply_card_drop(drag);
            }
        }
        if released {
            cx.notify();
        }
        released
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

    /// Drop the per-pane paint state for sessions that are no longer on screen.
    ///
    /// Keyed by session, so this is the same retirement the pane grids get: a
    /// closed tab must not leave its fade timer (or a stale drag) behind, and a
    /// retired pane's last painted rect must not keep answering hit tests for a
    /// region some other pane now occupies.
    fn retire_scrollbars(&mut self, live: &HashSet<SessionId>) {
        self.scrollbars.panes.retain(|session_id, _| live.contains(session_id));
        self.pane_bounds.retain(|session_id, _| live.contains(session_id));
        if self.scrollbars.drag.is_some_and(|session| !live.contains(&session)) {
            self.scrollbars.drag = None;
        }
    }

    /// The scrollbar placement for `session_id` against the last painted grid
    /// rect, or `None` when nothing has been painted or the session has no
    /// grid. Pointer hit-testing and drag math both resolve through this so
    /// they can never disagree with what paint drew.
    fn scrollbar_layout(&self, session_id: SessionId) -> Option<ScrollbarLayout> {
        let bounds = self.pane_grid_bounds(session_id)?;
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

    /// Track the pointer over every pane's scrollbar hit zone.
    ///
    /// Hover pins the overlay open and widens the thumb, which is what makes it
    /// grabbable: the resting 6 px thumb is a hint, and the 3x hit zone plus
    /// the widen are what turn it into a control. The pass covers all panes
    /// rather than the focused one because the wheel already scrolls whatever
    /// the pointer is over — a bar that fades in on an unfocused pane and then
    /// refuses to widen under the pointer is a control that lies. Sweeping all
    /// of them is also what clears the hover the pointer just left.
    fn update_scrollbar_hover(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let hovered = self.pane_at(position);
        let mut changed = false;
        for (session_id, state) in &self.scrollbars.panes {
            let width = state.borrow().current_width(self.scrollbars.style.width);
            let inside = hovered == Some(*session_id)
                && self.scrollbar_layout(*session_id).is_some_and(|layout| {
                    hit_test_scrollbar(
                        &layout,
                        f32::from(position.x),
                        f32::from(position.y),
                        width.max(self.scrollbars.style.width),
                    )
                });
            let mut state = state.borrow_mut();
            if inside == state.hover {
                continue;
            }
            if inside {
                state.on_hover_enter();
            } else {
                state.on_hover_leave();
            }
            changed = true;
        }
        if changed {
            cx.notify();
        }
    }

    /// Claim a left press that landed on any pane's scrollbar.
    ///
    /// A press on the thumb starts a drag; a press anywhere else in the hit
    /// zone jumps the viewport to that point on the track. Returns `true` when
    /// the press was consumed, so the caller leaves selection alone — the
    /// scrollbar is chrome painted over the grid, and a click on it was never
    /// meant for the cells underneath.
    ///
    /// Resolved by pointer, and ordered ahead of [`Self::press_focuses_pane`]
    /// for the same reason the wheel is: dragging a thumb is scrolling, and
    /// scrolling a pane you can see must not cost a focus-stealing click first.
    fn press_scrollbar(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) -> bool {
        let Some(session_id) = self.pane_at(position) else {
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
        let Some(pane) = self.pane_for(session_id) else { return };
        let moved = pane.with_terminal(|terminal| {
            terminal.set_split_scroll_eligibility(SplitScrollEligibility::default());
            terminal.scroll_to_offset(target)
        });
        if moved != Some(true) {
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
                this.close_find_overlay();
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
        let query = filter_terminal_image_placeholders(query);
        if query.is_empty() {
            return;
        }
        let Some(session_id) = self.shared.active_session.lock().ok().and_then(|guard| *guard)
        else {
            tracing::debug!("find query typed with no attached pane; nothing to search");
            return;
        };
        match self.sink.search_request(session_id, query.clone(), SEARCH_RESULT_LIMIT) {
            Ok(()) => tracing::info!(%session_id, %query, "sent search request"),
            Err(error) => tracing::warn!(%error, "search request dropped: IPC writer closed"),
        }
    }

    /// Drop the find overlay and release the snapshot the server cached for it.
    ///
    /// Every path that retires the overlay goes through here, because the
    /// server holds a full-scrollback snapshot of the searched session for as
    /// long as it believes the overlay is open (spec 017 US8-2).
    fn close_find_overlay(&mut self) {
        if self.find_overlay.take().is_some() {
            self.send_search_closed();
        }
    }

    /// Tell the server the find overlay closed, so it can drop the scrollback
    /// snapshot it was reusing across this session's query edits.
    ///
    /// Best-effort: the server drops the same snapshot on the session's next
    /// output, so a dropped release costs memory until then and nothing else.
    fn send_search_closed(&self) {
        let Some(session_id) = self.shared.active_session.lock().ok().and_then(|guard| *guard)
        else {
            return;
        };
        if let Err(error) = self.sink.search_closed(session_id) {
            tracing::debug!(%error, "search release dropped: IPC writer closed");
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
        // The open rows come from the same detector Ctrl+click uses, so the menu
        // can never offer to open something the pointer is not actually on — it
        // used to carry a hardcoded demo URI on every cell. No modifier is
        // required here: raising the menu is already an explicit request to act
        // on this cell. Exactly one of the three is ever set, since a cell
        // carries one span.
        let (url, file_path, osc8_uri) = match self.link_at(position) {
            Some(link) => match link.kind {
                SpanKind::Url => (Some(link.target), None, None),
                SpanKind::Path => (None, Some(link.target), None),
                SpanKind::Osc8Hyperlink => (None, None, Some(link.target)),
            },
            None => (None, None, None),
        };
        let request = ContextMenuRequest {
            has_selection: self
                .with_focused_grid(|terminal| terminal.has_selection())
                .unwrap_or(false),
            osc8_uri,
            url,
            file_path,
            smart_actions,
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
    /// The gate is pushed into the grid only when it differs from the one the
    /// published projection already reflects, which keeps an ordinary paint off
    /// the pane's own lock: the toggle moves on a config edit or a focus change,
    /// not on every frame.
    fn sync_split_scroll(&mut self) -> Arc<Content> {
        let eligibility = self.split_scroll_eligibility();
        let published = self.focused_session().and_then(|session_id| self.pane_frame(session_id));
        let content = match published {
            Some(frame) if frame.split_scroll == eligibility => Arc::clone(&frame.content),
            _ => self
                .with_focused_grid(|terminal| {
                    terminal.set_split_scroll_eligibility(eligibility);
                    terminal.content()
                })
                .unwrap_or_default(),
        };
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
        let Some(bounds) = self.focused_grid_bounds() else {
            return false;
        };
        let (rows, pin_rows) = self
            .with_focused_grid(|terminal| (terminal.content().rows.len(), terminal.pin_rows()))
            .unwrap_or((0, 0));
        hits_jump_chip(bounds, &self.font, rows, pin_rows, position)
    }

    /// The link under a window-space pointer position, if any.
    ///
    /// Resolved against the focused pane's recorded bounds, like every other
    /// pointer gesture on the grid: a press into an unfocused pane focuses it
    /// first, so by the time a link can be clicked its pane is the focused one.
    fn link_at(&self, position: Point<gpui::Pixels>) -> Option<HoveredLink> {
        let bounds = self.focused_grid_bounds()?;
        let cell = cell_at(bounds, &self.font, position)?;
        self.with_focused_grid(|terminal| terminal.link_at(cell))?
    }

    /// Open the link under a Ctrl+click, reporting whether there was one.
    ///
    /// Ordered ahead of mouse reporting in [`Self::press_grid`] for the same
    /// reason the scrollbar is: a modifier chord on a detected link is a
    /// gesture aimed at the terminal, not at the program running in it. The
    /// gate is narrow on purpose — with no link under the pointer the press
    /// falls straight through to the application, so Ctrl+click keeps working
    /// for programs that use it.
    ///
    /// The three kinds are deliberately not collapsed: an OSC 8 URI is
    /// program-supplied and goes through the scheme-allowlist gate that can
    /// raise the confirmation dialog, a heuristic URL keeps the silent
    /// non-allowlisted drop, and a path is not a URI at all — it is resolved
    /// against the pane's CWD and handed to the OS file handler.
    fn press_opens_link(&mut self, event: &MouseDownEvent, cx: &mut Context<Self>) -> bool {
        if !event.modifiers.control {
            return false;
        }
        let Some(link) = self.link_at(event.position) else { return false };
        self.pointer.drag = GridDrag::Link;
        tracing::info!(target = %link.target, "opening a Ctrl+clicked link");
        match link.kind {
            SpanKind::Osc8Hyperlink => self.route_osc8_activation(link.target, cx),
            SpanKind::Url => url_detect::open_url(&link.target),
            SpanKind::Path => url_detect::open_path(&link.target, self.focused_cwd().as_deref()),
        }
        true
    }

    /// The focused pane's OSC 7 working directory, which is what a relative
    /// path in that pane is relative *to*.
    ///
    /// `None` before the shell has reported one (no OSC 7 integration, or the
    /// first prompt has not been drawn yet), which leaves
    /// [`url_detect::open_path`] to hand the path over unresolved rather than
    /// guessing at the client process's own directory — that is Scribe's cwd,
    /// not the shell's, and opening something from it would be wrong more often
    /// than right.
    fn focused_cwd(&self) -> Option<PathBuf> {
        let session_id = self.focused_session()?;
        let metadata = self.shared.chrome_metadata.lock().ok()?;
        metadata.session(session_id)?.cwd.clone()
    }

    /// The live smart-selection rows for the grid cell under `position`.
    ///
    /// Empty whenever the pointer is off the grid, no rule matched, or the
    /// matched rules carry no actions — which is the ordinary case over blank
    /// space, so an ordinary right-click still gets the plain menu.
    fn smart_selection_rows(&self, position: Point<gpui::Pixels>) -> Vec<MenuItem> {
        let Some(bounds) = self.focused_grid_bounds() else {
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
    /// The restart-required flow starts a detached cold-restart helper, then
    /// asks every client window to flush its restore snapshot and exit. The
    /// helper waits for those exits before replacing the server and relaunching.
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
                match server_lifecycle::spawn_update_restart_helper() {
                    Ok(()) => {
                        tracing::info!("user approved deferred cold restart");
                        self.request_quit_all();
                    }
                    Err(error) => {
                        tracing::warn!(%error, "failed to spawn deferred update helper");
                    }
                }
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

    /// Give an armed Beads passage the key before panel Escape or PTY routing.
    fn handle_beads_editor_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let route =
            self.beads_editor.update(cx, |editor, editor_cx| editor.route_key(event, editor_cx));
        match route {
            BeadsEditorKeyRoute::Text => true,
            BeadsEditorKeyRoute::Consumed => {
                cx.stop_propagation();
                cx.notify();
                true
            }
            BeadsEditorKeyRoute::Finished => {
                cx.stop_propagation();
                window.focus(&self.focus.root, cx);
                cx.notify();
                true
            }
            BeadsEditorKeyRoute::Inactive => false,
        }
    }

    fn handle_modal_or_editor_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.window_displaced() || self.share_prompt_pending() {
            cx.stop_propagation();
            self.handle_overlay_key(event, cx);
            return true;
        }
        self.handle_beads_editor_key(event, window, cx)
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
        if event.keystroke.key == "escape"
            && self.shared.beads_panels.lock().is_ok_and(|mut panels| panels.dismiss_latest())
        {
            cx.notify();
            return true;
        }
        let overlay_free = self.dialog.is_none()
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
        self.update_window_activation(active, cx);
        if !active {
            return;
        }
        if let Some(handle) = window.window_handle().downcast::<TerminalView>() {
            RECENT_TERMINAL_WINDOW.with(|recent| recent.replace(Some(handle)));
        }
        let window_id = self.shared.lifecycle.lock().ok().and_then(|state| state.window_id());
        report_terminal_activation(window_id);
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

    /// Apply one window-activation state across every focus consumer.
    fn update_window_activation(&mut self, active: bool, cx: &mut Context<Self>) {
        self.focus.cursor_blink.set_window_active(active);
        cx.notify();
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
    }

    /// Refresh the X11 active-window guard's cached state (no-op off X11).
    ///
    /// Driven by [`drive_x11_focus_polls`] so an overlay that opens and closes
    /// between keystrokes still arms the reactivation debounce.
    fn poll_x11_focus(&mut self, cx: &mut Context<Self>) {
        let x11_active = self.x11_focus.as_mut().and_then(X11FocusGuard::poll);
        let window_active =
            self.shared.lifecycle.lock().is_ok_and(|lifecycle| lifecycle.window_active());
        if should_reconcile_window_activation(window_active, x11_active) {
            // Only repair a stale blur from confirmed EWMH truth. Inactive
            // observations still belong exclusively to the overlay input gate.
            self.update_window_activation(true, cx);
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
            tracing::warn!(%error, "keystroke refused");
            self.report_refused_input(error);
            return;
        }
        // Perf gate: start the echo round-trip clock the PTY-output path stops.
        scribe_common::perf_probe::record_input_sent(session_id);
    }

    /// Put a refused input frame on the window status bar.
    ///
    /// The bounded outbound queue refuses rather than evicts, so a refusal is a
    /// keystroke the user typed and the server will never see. Logging it is not
    /// enough: the status bar is a live region, so the refusal is announced as
    /// well as painted, and the user learns the pane is deaf instead of assuming
    /// the command they typed went through.
    fn report_refused_input(&self, error: SinkError) {
        set_status(&self.shared.status, &self.shared.generation, format!("input refused: {error}"));
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
            .map(|tabs| tabs.entries().map(|entry| entry.session_id).collect())
            .unwrap_or_default();
        let focused = self.shared.active_session.lock().ok().and_then(|guard| *guard);
        scribe_common::perf_probe::record_sessions(sessions, focused);
    }
}

impl Drop for TerminalView {
    fn drop(&mut self) {
        if !self.process_shutdown_finished {
            self.process_shutdown_finished = true;
            self.shared.process_shutdown.finish_view();
        }
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
/// actions: closing this window (or quitting the app on a quit-all), reporting
/// focus, and re-polling the window list.
async fn drive_window_lifecycle(view: WeakEntity<TerminalView>, app: &mut AsyncApp) {
    loop {
        app.background_executor().timer(WINDOW_LIFECYCLE_TICK).await;
        if view.update_in(app, TerminalView::poll_window_lifecycle).is_err() {
            return;
        }
    }
}

/// The redraw pump's tick, and therefore the interval one paced burst is
/// presented on: the drain and [`run_frame_pacer`] hand the grid one committed
/// burst per turn of this clock, so "one burst per redraw" is a single number
/// rather than two that can drift apart.
const REDRAW_INTERVAL: Duration = Duration::from_millis(16);

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
        app.background_executor().timer(REDRAW_INTERVAL).await;
        let current = generation.load(Ordering::Acquire);
        if current == rendered {
            let idle = view.update(app, |view, view_cx| {
                view.expire_share_hint(view_cx);
                view.poll_scrollbar_fades(view_cx);
                view.tick_ai_animation(view_cx);
                view.tick_cursor_blink(view_cx);
            });
            if idle.is_err() {
                return;
            }
            continue;
        }
        rendered = current;
        if view
            .update(app, |view, view_cx| {
                view.tick_cursor_blink(view_cx);
                view_cx.notify();
            })
            .is_err()
        {
            return;
        }
    }
}

fn ci_action_ids(workspace_id: WorkspaceId) -> (ElementId, ElementId) {
    (
        ElementId::from(format!("ci-open-{}", workspace_id.to_full_string())),
        ElementId::from(format!("ci-dismiss-{}", workspace_id.to_full_string())),
    )
}

fn visible_ci_run(
    runs: &CiRunBars,
    workspace_id: WorkspaceId,
    root: PathBuf,
) -> Option<VisibleCiRun> {
    let state = runs.get(&root).cloned()?;
    let details = runs.details(&root, &state.head_sha).cloned();
    Some((workspace_id, root, state, details))
}

fn ci_panel_height(details: Option<&CiRunDetails>, stale: bool) -> f32 {
    details.map_or(ci_bar::CI_TRACE_LOADING_HEIGHT, |details| {
        ci_bar::CiTraceModel::build(details, 0, stale).height()
    })
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
        // The tab strip is the only place the window's live session count and
        // the attached pane's workspace are both known, and it is a different
        // lock from the metadata store, so resolve the count and the id (a
        // `Copy`) and release it before reading the names. The count mirrors
        // the legacy client's pane count: every session open in this window,
        // not just the attached one.
        let (session_count, workspace_id) = self.shared.tabs.lock().map_or((0, None), |tabs| {
            let workspace_id = active_session.and_then(|session_id| tabs.workspace_of(session_id));
            (tabs.len(), workspace_id)
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

    /// Build the status-bar band, wiring every update CTA activation path to
    /// the confirmation modal.
    ///
    /// The palette is cached across renders, so the live opacity is folded into
    /// the filled band here rather than at theme-reload time.
    fn render_status_bar(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let model = self.build_status_model();
        let colors = self.status_colors.with_opacity(self.opacity);
        let update_view = cx.entity().downgrade();
        let on_update = Box::new(move |_window: &mut Window, app: &mut App| {
            if let Err(error) = update_view.update(app, TerminalView::open_update_dialog) {
                tracing::debug!(?error, "update CTA activation dropped with its view");
            }
        });
        let settings_view = cx.entity().downgrade();
        let on_settings = Box::new(move |_window: &mut Window, app: &mut App| {
            if let Err(error) = settings_view.update(app, TerminalView::open_or_focus_settings) {
                tracing::debug!(?error, "settings gear activation dropped with its view");
            }
        });
        // The balance button only earns its corner once there is more than
        // one surface to balance.
        let equalize_view = cx.entity().downgrade();
        let equalize_action: status_bar::UpdateActionHandler =
            Box::new(move |_window: &mut Window, app: &mut App| {
                if let Err(error) = equalize_view.update(app, TerminalView::equalize_layout) {
                    tracing::debug!(?error, "balance button activation dropped with its view");
                }
            });
        let on_equalize = (self.shell.pane_count(cx) >= 2).then_some(equalize_action);
        status_bar::render(
            &model,
            window_chrome::STATUS_BAR_HEIGHT,
            &colors,
            status_bar::StatusBarActions {
                update_focus: Some(&self.focus.update),
                on_update: Some(on_update),
                on_equalize,
                on_settings: Some(on_settings),
            },
        )
        .into_any_element()
    }

    /// Build one pane's prompt-bar model, or `None` when the bar is disabled
    /// or that pane's session has no prompts yet.
    ///
    /// The bar is per-pane chrome: [`Self::render_panes`] calls this for every
    /// visible pane so each AI session's prompts render inside the pane that
    /// runs it, never across its neighbours.
    ///
    /// The context meter is attached whenever the tracker holds a percentage for
    /// the pane, independent of the warn band — the prompt bar is the surface
    /// that always shows the Ok band, while the tab suffix suppresses it (see
    /// [`Self::sync_tab_context_suffix`]).
    fn prompt_model_for(&self, session_id: SessionId) -> Option<prompt_bar::PromptBarModel> {
        if !self.prompt_bar.enabled {
            return None;
        }
        let ai = self.shared.ai.lock().ok()?;
        let data = ai.visible_prompts(session_id)?;
        let indicator = ai.tracker.context_for(session_id).map(|percent| {
            PromptContextIndicator::from_thresholds(
                percent,
                &self.context_thresholds,
                self.prompt_colors.text,
            )
        });
        prompt_bar::build_model(data, std::time::SystemTime::now(), indicator)
    }

    /// The glyph size and row cell height every prompt strip paints at.
    ///
    /// Read from the live grid font, so the strip matches the terminal text
    /// beside it and follows both an `appearance.font_size` edit and a zoom
    /// step; `terminal.prompt_bar_font_size` overrides it when set. The single
    /// source for [`Self::pane_prompt_bar_height`] (what the pane reserves) and
    /// [`Self::compose_pane_content`] (what is painted), which must agree or the
    /// PTY grid is sized against a band that is not there.
    fn prompt_bar_metrics(&self) -> prompt_bar::PromptBarMetrics {
        prompt_bar::PromptBarMetrics::resolve(
            self.prompt_bar.font_size,
            self.font.size,
            self.font.line_height,
            self.font.cell_width(),
        )
    }

    /// Interactive wiring for one pane's strip: the view's hover state on the
    /// way in, and the listeners that keep it current on the way out.
    ///
    /// The handlers hold a weak view handle rather than a listener closure
    /// because the strip is built from a `&self` render helper, the same shape
    /// [`Self::render_status_bar`] uses for its CTA and gear.
    fn prompt_bar_actions(
        &self,
        session_id: SessionId,
        width: f32,
        cx: &Context<Self>,
    ) -> prompt_bar::PromptBarActions {
        let hover_view = cx.entity().downgrade();
        let dismiss_view = cx.entity().downgrade();
        prompt_bar::PromptBarActions {
            id: gpui::ElementId::Name(format!("ai-prompt-status-{session_id}").into()),
            hover: self
                .pointer
                .prompt_hover
                .filter(|(hovered, _)| *hovered == session_id)
                .map(|(_, target)| target),
            width,
            on_hover: Rc::new(move |target, hovered, _window: &mut Window, app: &mut App| {
                if let Err(error) = hover_view.update(app, |view, ctx| {
                    view.set_prompt_hover(session_id, target, hovered, ctx);
                }) {
                    tracing::debug!(?error, "prompt-bar hover dropped with its view");
                }
            }),
            on_dismiss: Box::new(move |_window: &mut Window, app: &mut App| {
                if let Err(error) =
                    dismiss_view.update(app, |view, ctx| view.dismiss_prompt_bar(session_id, ctx))
                {
                    tracing::debug!(?error, "prompt-bar dismissal dropped with its view");
                }
            }),
        }
    }

    /// Record (or clear) the strip target the pointer is over.
    ///
    /// A leave only clears the hover it is actually leaving: GPUI does not
    /// promise the old row's `false` arrives before the new row's `true`, and
    /// clearing unconditionally would drop the fresh hover when the pointer
    /// slides from the first row onto the latest.
    fn set_prompt_hover(
        &mut self,
        session_id: SessionId,
        target: prompt_bar::PromptBarHover,
        hovered: bool,
        cx: &mut Context<Self>,
    ) {
        let entry = (session_id, target);
        let next = if hovered {
            Some(entry)
        } else if self.pointer.prompt_hover == Some(entry) {
            None
        } else {
            self.pointer.prompt_hover
        };
        if self.pointer.prompt_hover != next {
            self.pointer.prompt_hover = next;
            cx.notify();
        }
    }

    /// Hide `session_id`'s prompt bar, giving its rows back to the PTY grid.
    ///
    /// No explicit resize call: the render pass republishes every pane's
    /// geometry from [`Self::pane_prompt_bar_height`], which now reports zero
    /// for this session, so the redraw this notify schedules resizes the PTY.
    fn dismiss_prompt_bar(&mut self, session_id: SessionId, cx: &mut Context<Self>) {
        if let Ok(mut ai) = self.shared.ai.lock() {
            ai.dismiss(session_id);
        } else {
            tracing::warn!("AI chrome mutex poisoned; prompt-bar dismissal dropped");
        }
        self.pointer.prompt_hover = None;
        cx.notify();
    }

    /// Height of the prompt strip `session_id`'s pane reserves, or zero when
    /// the feature is off or the session has no prompts.
    ///
    /// [`Self::publish_pane_sizes`] subtracts this from the pane rect before
    /// deriving the PTY grid, so the terminal is exactly the rows the strip
    /// leaves visible — the winit client's resize-on-bar-change behaviour.
    fn pane_prompt_bar_height(&self, session_id: SessionId) -> f32 {
        if !self.prompt_bar.enabled {
            return 0.0;
        }
        let metrics = self.prompt_bar_metrics();
        let Ok(ai) = self.shared.ai.lock() else { return 0.0 };
        ai.visible_prompts(session_id)
            .map_or(0.0, |data| prompt_bar::prompt_bar_height(data.prompts.prompt_count, metrics))
    }

    /// The invisible canvas that measures the grid band and republishes the
    /// pane geometry whenever the band moved.
    ///
    /// The band's height is whatever the chrome bands leave over, which no
    /// arithmetic on the window size can predict, so the painted rect is the
    /// only honest source for the cell counts the server is told about. (The
    /// prompt strip is pane-internal chrome and never moves this band; its
    /// height is folded in per pane by [`Self::publish_pane_sizes`].)
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
    fn render_grid(
        &mut self,
        ime: ImePaint,
        link: Option<HoveredLink>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let content = self.sync_split_scroll();
        let appearance = &self.config.config().config.appearance;
        let cursor = CursorPaint {
            visible: self.focus.cursor_blink.window_active
                && (!appearance.cursor_blink || self.focus.cursor_blink.visible),
            shape: appearance.cursor_shape,
        };
        let panes = self.render_panes(FocusedPanePaint { content, ime, cursor, link }, cx);
        let dividers = self.render_dividers(cx);
        let region_bars = self.render_region_tab_bars(cx);
        let ci_bars = self.render_ci_run_bars(cx);
        // The board is placed against the same region rects the panes are, so
        // it belongs in the grid band with them rather than in a window-wide
        // band that would span every region.
        let beads_boards = self.render_beads_boards(cx);
        let beads_panels = self.render_beads_panels(cx);
        div()
            .id("terminal-grid")
            .flex_1()
            .relative()
            .bg(surface(self.terminal_colors.background, self.opacity))
            .child(self.grid_area_probe(cx))
            .children(panes)
            .children(region_bars)
            .children(ci_bars)
            .children(beads_boards)
            .children(beads_panels)
            // Dividers paint last, over panes, region bars, and boards alike:
            // a region divider runs the full height of the split, so two
            // regions with boards open must still read as two regions.
            .children(dividers)
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
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|view, _event: &MouseUpEvent, _window, ctx| {
                    view.release_board(ctx);
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

    /// Publish the strip each pinned board takes out of its own region, before
    /// the geometry passes that turn region rects into published PTY sizes.
    fn sync_beads_board_strips(&mut self, cx: &App) {
        let live = self.shell.region_workspaces(cx);
        let mut copied = None;
        let mut panel_copy = None;
        let mut detail_requests = Vec::new();
        let mut issue_writes = Vec::new();
        let panel_workspaces = self
            .shared
            .beads_panels
            .lock()
            .map(|mut panels| {
                panels.expire_notices();
                panels.retain_regions(&live);
                panel_copy = panels.take_copy();
                while let Some(request) = panels.take_request() {
                    detail_requests.push(request);
                }
                while let Some(write) = panels.take_write() {
                    issue_writes.push(write);
                }
                panels.workspaces()
            })
            .unwrap_or_default();
        let mut strips = HashMap::new();
        self.visible_beads_boards = self
            .shared
            .beads_boards
            .lock()
            .map(|mut boards| {
                boards.retain_regions(&live);
                copied = boards.take_copy();
                let mut visible = boards.visible();
                include_open_panel_boards(&mut visible, &panel_workspaces, &boards);
                strips = visible
                    .iter()
                    .filter(|(_, pinned)| *pinned)
                    .map(|(workspace_id, _)| (*workspace_id, boards.height(*workspace_id)))
                    .collect();
                visible
            })
            .unwrap_or_default();
        // A card asked for an id or an epic name; the board itself cannot
        // reach the window's clipboard handle, so the copy lands here.
        if let Some(text) = copied {
            self.write_clipboard(text);
        }
        if let Some(text) = panel_copy {
            self.write_clipboard(text);
        }
        for (workspace_id, issue_id) in detail_requests {
            if let Err(error) = self.sink.request_beads_issue_detail(workspace_id, issue_id) {
                tracing::debug!(%error, "Beads issue detail request dropped");
            }
        }
        for write in issue_writes {
            self.send_beads_issue_write(write);
        }
        self.shell.set_pinned_boards(strips);
    }

    fn send_beads_issue_write(&self, write: PanelWriteIntent) {
        let workspace_id = write.workspace_id;
        let issue_id = write.issue_id;
        let result =
            self.sink.write_beads_issue(workspace_id, issue_id.clone(), write.verb, write.guards);
        let Err(error) = result else { return };
        tracing::debug!(%error, %workspace_id, %issue_id, "Beads issue write dropped");
        if let Ok(mut boards) = self.shared.beads_boards.lock() {
            boards.cancel_card_drop(workspace_id, &issue_id);
        }
        if let Ok(mut panels) = self.shared.beads_panels.lock() {
            panels.write_send_failed(workspace_id, &issue_id, &error.to_string());
        }
        self.shared.generation.fetch_add(1, Ordering::Release);
    }

    /// Match repository-keyed CI snapshots to the regions that own them.
    fn sync_ci_run_strips(&mut self, cx: &mut Context<Self>) {
        let roots = self.shell.region_project_roots(cx);
        self.visible_ci_runs = self.shared.ci_runs.lock().map_or_else(
            |_| {
                tracing::warn!("CI run state mutex poisoned; hiding bands");
                Vec::new()
            },
            |runs| {
                roots
                    .into_iter()
                    .filter_map(|(workspace_id, root)| visible_ci_run(&runs, workspace_id, root))
                    .collect()
            },
        );
        let visible = self
            .visible_ci_runs
            .iter()
            .map(|(workspace_id, repo_root, state, _)| {
                (*workspace_id, repo_root.clone(), state.head_sha.clone())
            })
            .collect::<HashSet<_>>();
        let closed = self
            .ci_expanded
            .iter()
            .filter(|(workspace_id, (repo_root, head_sha))| {
                !visible.contains(&(**workspace_id, repo_root.clone(), head_sha.clone()))
            })
            .map(|(workspace_id, request)| (*workspace_id, request.clone()))
            .collect::<Vec<_>>();
        for (workspace_id, (repo_root, head_sha)) in closed {
            self.ci_expanded.remove(&workspace_id);
            self.set_ci_detail_interest(repo_root, head_sha, false);
        }
        let visible_ids =
            visible.iter().map(|(workspace_id, ..)| *workspace_id).collect::<HashSet<_>>();
        self.ci_action_focus.retain(|workspace_id, _| visible_ids.contains(workspace_id));
        for workspace_id in &visible_ids {
            self.ensure_ci_action_focus(*workspace_id, cx);
        }
        self.shell.set_ci_strips(
            self.visible_ci_runs
                .iter()
                .map(|(workspace_id, repo_root, state, details)| {
                    let expanded = self
                        .ci_expanded
                        .get(workspace_id)
                        .is_some_and(|open| open.0 == *repo_root && open.1 == state.head_sha);
                    let panel =
                        if expanded { ci_panel_height(details.as_ref(), state.stale) } else { 0.0 };
                    (*workspace_id, ci_bar::CI_BAR_HEIGHT + panel)
                })
                .collect(),
        );
    }

    fn ensure_ci_action_focus(&mut self, workspace_id: WorkspaceId, cx: &mut Context<Self>) {
        if self.ci_action_focus.contains_key(&workspace_id) {
            return;
        }
        self.ci_action_focus.insert(
            workspace_id,
            (
                cx.focus_handle().tab_index(0).tab_stop(true),
                cx.focus_handle().tab_index(0).tab_stop(true),
                cx.focus_handle().tab_index(0).tab_stop(true),
            ),
        );
    }

    fn ci_open_handler(url: String) -> ci_bar::CiActionHandler {
        Arc::new(move |_: &mut Window, _: &mut App| url_detect::open_url(&url))
    }

    fn ci_dismiss_handler(&self, repo_root: PathBuf, head_sha: String) -> ci_bar::CiActionHandler {
        let sink = self.sink.clone();
        Arc::new(move |_: &mut Window, _: &mut App| {
            if let Err(error) = sink.dismiss_ci_run(repo_root.clone(), head_sha.clone()) {
                tracing::warn!(%error, "CI run dismissal dropped: IPC writer closed");
            }
        })
    }

    fn set_ci_detail_interest(&self, repo_root: PathBuf, head_sha: String, interested: bool) {
        if let Err(error) = self.sink.set_ci_run_details_interest(repo_root, head_sha, interested) {
            tracing::warn!(%error, "CI detail interest dropped: IPC writer closed");
        }
    }

    // @lat: [[client#GPUI CI Run Bar#Demand-driven job data]]
    fn toggle_ci_trace(
        &mut self,
        workspace_id: WorkspaceId,
        repo_root: PathBuf,
        head_sha: String,
        cx: &mut Context<Self>,
    ) {
        if self
            .ci_expanded
            .get(&workspace_id)
            .is_some_and(|open| open.0 == repo_root && open.1 == head_sha)
        {
            self.ci_expanded.remove(&workspace_id);
            self.set_ci_detail_interest(repo_root, head_sha, false);
        } else {
            if let Some((old_root, old_head)) =
                self.ci_expanded.insert(workspace_id, (repo_root.clone(), head_sha.clone()))
            {
                self.set_ci_detail_interest(old_root, old_head, false);
            }
            self.set_ci_detail_interest(repo_root, head_sha, true);
        }
        cx.notify();
    }

    fn ci_toggle_handler(
        workspace_id: WorkspaceId,
        repo_root: PathBuf,
        head_sha: String,
        cx: &Context<Self>,
    ) -> ci_bar::CiActionHandler {
        let view = cx.weak_entity();
        Arc::new(move |_: &mut Window, app: &mut App| {
            view.update(app, |view, view_cx| {
                view.toggle_ci_trace(workspace_id, repo_root.clone(), head_sha.clone(), view_cx);
            })
            .ok();
        })
    }

    /// Paint one collapsed trace band in each region whose repository has CI state.
    fn render_ci_run_bars(&self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let colors = CiBarColors::from_theme(&self.config.config().theme, self.opacity);
        let animations = AnimationSettings::from_config(&self.config.config().config);
        let viewport = self.pane_viewport();
        self.visible_ci_runs
            .iter()
            .filter_map(|(workspace_id, repo_root, state, details)| {
                let rect = self.shell.ci_bar_rect(*workspace_id, viewport, cx)?;
                let model = CiBarModel::build(state, now, self.shared.ci_owner_controls);
                let (open_id, dismiss_id) = ci_action_ids(*workspace_id);
                let (toggle_focus, open_focus, dismiss_focus) = self
                    .ci_action_focus
                    .get(workspace_id)
                    .cloned()
                    .map(|(toggle, open, dismiss)| (toggle, Some(open), Some(dismiss)))?;
                let expanded = self
                    .ci_expanded
                    .get(workspace_id)
                    .is_some_and(|open| open.0 == *repo_root && open.1 == state.head_sha);
                let trace_now = if state.stale {
                    state
                        .workflows
                        .iter()
                        .filter_map(|workflow| workflow.updated_at_epoch_secs)
                        .max()
                        .unwrap_or(now)
                } else {
                    now
                };
                let trace = expanded
                    .then_some(details.as_ref())
                    .flatten()
                    .map(|details| ci_bar::CiTraceModel::build(details, trace_now, state.stale));
                let on_open = model.open_url.clone().map(Self::ci_open_handler);
                let on_dismiss = self
                    .shared
                    .ci_owner_controls
                    .then(|| self.ci_dismiss_handler(repo_root.clone(), model.head_sha.clone()));
                let on_toggle = Self::ci_toggle_handler(
                    *workspace_id,
                    repo_root.clone(),
                    model.head_sha.clone(),
                    cx,
                );
                Some(ci_bar::render(
                    &model,
                    &colors,
                    ci_bar::CiBarRender {
                        id: gpui::ElementId::Name(format!("ci-run-{workspace_id}").into()),
                        trace_id: gpui::ElementId::Name(format!("ci-trace-{workspace_id}").into()),
                        open_id,
                        dismiss_id,
                        rect,
                        accent: self.shell.workspace_accent(*workspace_id, cx),
                        animations,
                        expanded,
                        trace,
                        toggle_focus,
                        open_focus,
                        dismiss_focus,
                        on_toggle,
                        on_open,
                        on_dismiss,
                    },
                ))
            })
            .collect()
    }

    // @lat: [[client#Client#Beads Board CLI Data Source]]
    fn render_beads_boards(&mut self, cx: &App) -> Vec<gpui::AnyElement> {
        let Ok(boards) = self.shared.beads_boards.lock() else { return Vec::new() };
        let scale = boards.text_scale();
        let colors = self.beads_colors;
        let viewport = self.pane_viewport();
        self.visible_beads_boards
            .iter()
            .filter_map(|(workspace_id, pinned)| {
                let rect = self.shell.board_rect(
                    *workspace_id,
                    boards.height(*workspace_id),
                    viewport,
                    cx,
                )?;
                let name = self.workspace_name(*workspace_id).unwrap_or_else(|| "workspace".into());
                Some((
                    scribe_client::beads_board::render(
                        &name,
                        boards.state(*workspace_id),
                        BeadsBoardRender {
                            rect,
                            overlay: !pinned,
                            hover_state: Arc::clone(&self.shared.beads_boards),
                            panel_state: Arc::clone(&self.shared.beads_panels),
                            workspace_id: *workspace_id,
                            drag_target: boards.drag_target(*workspace_id),
                            scale,
                            colors,
                        },
                    ),
                    // The bar's grab band, carrying nothing but the pointer a
                    // divider's does — the press itself is resolved against the
                    // same rect over in `press_board_edge`, so the two cannot
                    // disagree about where the bar is.
                    div()
                        .absolute()
                        .left(px(rect.x))
                        .top(px(rect.y + rect.height - BEADS_BOARD_GRIP))
                        .w(px(rect.width))
                        .h(px(BEADS_BOARD_GRIP * 2.0))
                        .cursor(gpui::CursorStyle::ResizeUpDown)
                        .into_any_element(),
                ))
            })
            .flat_map(|(board, grip)| [board, grip])
            .collect()
    }

    fn render_beads_panels(&self, cx: &App) -> Vec<gpui::AnyElement> {
        let (layers, write_enabled) = self
            .shared
            .beads_panels
            .lock()
            .map(|panels| {
                let open = panels
                    .workspaces()
                    .into_iter()
                    .map(|workspace_id| {
                        (
                            workspace_id,
                            panels.visible(workspace_id).cloned(),
                            panels.notice(workspace_id).map(str::to_owned),
                            panels.notice_lane(workspace_id),
                        )
                    })
                    .collect::<Vec<_>>();
                (open, panels.write_enabled())
            })
            .unwrap_or_default();
        let Ok(boards) = self.shared.beads_boards.lock() else { return Vec::new() };
        let viewport = self.pane_viewport();
        let animations = AnimationSettings::from_config(&self.config.config().config);
        layers
            .into_iter()
            .flat_map(|(workspace_id, panel, notice, notice_lane)| {
                let Some(region) = self.shell.workspace_rect(workspace_id, viewport, cx) else {
                    return Vec::new();
                };
                let Some(board) =
                    self.shell.board_rect(workspace_id, boards.height(workspace_id), viewport, cx)
                else {
                    return Vec::new();
                };
                let wiring = BeadsPanelRender {
                    region,
                    board,
                    workspace_id,
                    state: Arc::clone(&self.shared.beads_panels),
                    editor: self.beads_editor.clone(),
                    terminal_focus: self.focus.root.clone(),
                    app: cx,
                    write_enabled,
                    scale: boards.text_scale(),
                    colors: self.beads_colors,
                    animations,
                };
                let mut elements = panel
                    .as_ref()
                    .map_or_else(Vec::new, |panel| beads_panel::render(panel, &wiring));
                if let (Some(text), Some(lane)) = (notice, notice_lane)
                    && let Some(notice_element) = beads_panel::render_notice(&text, lane, &wiring)
                {
                    elements.push(notice_element);
                }
                elements
            })
            .collect()
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

    /// Build the active remote-connect picker above normal terminal chrome.
    fn build_remote_picker_overlay(&self) -> Option<gpui::AnyElement> {
        self.remote_connect.is_active().then(|| {
            let colors = RemotePickerColors::from(&self.chrome);
            remote_picker_overlay(&self.remote_connect.view(), &colors)
        })
    }

    /// Keep keyboard focus inside a modal, or restore it to terminal chrome
    /// when no modal owns the window.
    fn ensure_focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(dialog) = self.dialog.as_ref() {
            let focus = dialog.focus_handle(cx);
            if !focus.is_focused(window) {
                window.focus(&focus, cx);
            }
            return;
        }
        let focus_is_unclaimed = focus_is_unclaimed([
            self.focus.root.is_focused(window),
            self.titlebar.read(cx).has_keyboard_focus(window),
            self.focus.update.is_focused(window),
            self.ci_action_focus.values().any(|(toggle, open, dismiss)| {
                toggle.is_focused(window) || open.is_focused(window) || dismiss.is_focused(window)
            }),
            self.beads_editor.read(cx).has_keyboard_focus(window, cx),
        ]);
        if focus_is_unclaimed {
            window.focus(&self.focus.root, cx);
        }
    }

    /// Whether chrome Tab traversal claims this keystroke.
    ///
    /// Plain Tab / Shift+Tab continue the tab-stop order only while a chrome
    /// control already owns keyboard focus. With the terminal root focused the
    /// keystroke belongs to the PTY — Tab completion is core terminal
    /// behavior — so it falls through to the encoder (`\t`, `ESC [ Z` for
    /// Shift+Tab). Modified Tab chords (and Ctrl+I, a distinct keystroke that
    /// encodes to the same byte) never mean traversal.
    fn traversal_claims_tab(event: &KeyDownEvent, terminal_focused: bool) -> bool {
        let modifiers = event.keystroke.modifiers;
        !terminal_focused
            && event.keystroke.key == "tab"
            && !modifiers.control
            && !modifiers.alt
            && !modifiers.platform
    }

    fn focus_next_titlebar_control(
        &self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !Self::traversal_claims_tab(event, self.focus.root.is_focused(window)) {
            return false;
        }
        if event.keystroke.modifiers.shift {
            window.focus_prev(cx);
        } else {
            window.focus_next(cx);
        }
        true
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        log_first_frame_timing();
        self.report_perf_frame();
        self.ensure_focus(window, cx);
        self.apply_saved_position(window);
        self.apply_saved_window_state(window);
        self.apply_saved_desktop(window);
        self.apply_pending_minimize(window);
        // Driven from the frame loop rather than only from the bounds observer:
        // the observer stops firing once the window settles, and the check
        // deliberately waits for that. The focused window's cursor blink keeps
        // frames coming well inside the wait.
        self.verify_restored_position(window);
        self.capture_geometry(window, cx);
        self.sync_tabs(cx);
        self.sync_find_results(cx);
        self.sync_remote_connect();
        self.reconcile_panes(cx);
        self.sync_equalize_visibility(cx);
        self.sync_ci_run_strips(cx);
        self.sync_beads_board_strips(cx);
        self.sync_grid_geometry(cx);
        // A prompt or CI state edge changes an internal strip without moving the
        // grid band, so the band probe never notices; republishing here keeps the
        // affected PTYs exactly the rows their strips leave visible.
        // Per-session no-change checks inside make this idempotent.
        self.publish_pane_sizes(cx);
        let ime = self.sync_ime(cx);
        // The hovered link is read straight off the window rather than tracked
        // across events: the pointer position and the modifier state are both
        // already live there, so a hover that survives a repaint needs no state
        // of its own to survive with it.
        let link =
            window.modifiers().control.then(|| self.link_at(window.mouse_position())).flatten();
        let grid = self.render_grid(ime, link, cx);
        let status_bar = self.render_status_bar(cx);
        let tooltip = self.build_tooltip_demo();
        let share = self.build_share_overlay();
        let remote_picker = self.build_remote_picker_overlay();
        let displaced = self.build_lost_control_overlay(cx);
        // The root itself paints nothing. Every band below fills the window
        // edge to edge, so leaving the root unfilled guarantees each pixel
        // carries the opacity alpha exactly once instead of compositing a
        // translucent band over a translucent root and coming out more opaque
        // than the configured value.
        div()
            .track_focus(&self.focus.root)
            // Ctrl going down or up changes what the grid shows with no pointer
            // motion behind it, and an idle pane bumps no generation, so the
            // redraw pump would never come round to notice. This is the only
            // thing that rules a link under a stationary pointer.
            //
            // It sits on the focus root rather than on the grid band because
            // gpui dispatches modifier changes down the *focus* path, like key
            // events — a listener on the hovered-but-unfocusable band is never
            // reached at all.
            .on_modifiers_changed(cx.listener(|_view, _: &ModifiersChangedEvent, _win, ctx| {
                ctx.notify();
            }))
            .on_action(cx.listener(
                |view, _: &CloseWindow, _window, ctx| {
                    view.request_window_close(ctx);
                },
            ))
            .on_key_down(cx.listener(|view, event: &KeyDownEvent, win, ctx| {
                if view.focus.cursor_blink.show_now() {
                    ctx.notify();
                }
                // The X11 active-window guard gates everything: while a
                // compositor overlay owns the screen the keystroke was never
                // meant for this window, so it reaches no consumer at all.
                if view.compositor_overlay_active(event) {
                    ctx.stop_propagation();
                    return;
                }
                if view.handle_modal_or_editor_key(event, win, ctx) {
                    return;
                }
                ctx.stop_propagation();
                // An active overlay owns every key, including plain Tab. With
                // no overlay, plain Tab continues the chrome tab-stop order
                // only while chrome already has focus (the status-bar update
                // CTA is the one stop that bubbles here — titlebar controls
                // stop propagation in their own handlers). A focused terminal
                // keeps Tab for the PTY; modified Tab chords continue to
                // configured bindings below.
                if view.handle_overlay_key(event, ctx) {
                    return;
                }
                if view.focus_next_titlebar_control(event, win, ctx) {
                    return;
                }
                // Vi mode and configured bindings run before the generic PTY
                // byte encoder.
                if view.handle_vi_key(event, ctx) || view.handle_binding(event, ctx) {
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
            // `flex_none` on every band below the grid: the grid is the one
            // flex-grown child, so without it a window shorter than the grid's
            // painted height would shrink the bands away instead of clipping
            // the grid, taking the status surfaces off screen.
            .child(status_bar)
            .children(self.command_palette.clone())
            .children(self.find_overlay.clone())
            .children(self.context_menu.clone())
            .children(self.dialog.clone())
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
/// 18.9 px) with nothing left for the titlebar and status bar, so the bottom
/// rows were cut off and a slightly smaller window would have taken the bands
/// with them. The result is clamped to the
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

/// Which window a backend is being started for.
///
/// The three answers travel together because they are one decision: a window
/// that names an existing window inherits its sessions and must not add a shell,
/// and only the process bootstrap — the one window that stands for the whole
/// client — speaks for the windows it did not open.
#[derive(Clone, Copy)]
struct WindowBackend {
    /// The window this connection's `Hello` claims: the resumed window's id for
    /// the bootstrap, a freshly minted one for a deliberate new window, the id
    /// the server named in `other_windows` for a restored sibling, or
    /// `SCRIBE_JOIN_WINDOW`'s for a share join. `None` asks the server to hand
    /// back any window whose sessions outlived their client.
    claim: Option<WindowId>,
    /// Whether `claim` came from the explicit local share-join launch path.
    join_intent: LocalJoinIntent,
    /// Whether this window brings its own first login shell.
    initial_session: bool,
    /// Whether this connection may reopen the other windows `Welcome` reports.
    fan_out: bool,
}

#[derive(Clone, Copy)]
enum LocalJoinIntent {
    Plain,
    Join,
}

/// Build one terminal window's backend: fresh shared state plus its own IPC
/// connection to the server, with the reader/writer thread already running.
///
/// Every window owns an independent [`Shared`] — its own grid, status line, tab
/// strip, and chrome — because a window is a separate client from the server's
/// point of view. Sharing one `Shared` between windows would instead mirror a
/// single session strip into both, which is not what "new window" means.
///
/// Called once from [`main`] for the startup window and again for every window
/// this process opens afterwards, so the paths cannot drift.
fn start_window_backend(terminal_size: TerminalSize, window: WindowBackend) -> (Shared, IpcSink) {
    let WindowBackend { claim, join_intent, initial_session, fan_out } = window;
    let ci_owner_controls = matches!(join_intent, LocalJoinIntent::Plain)
        && lan_dial::target_from_env().is_none()
        && remote_handshake::target_from_env().is_none();
    let Some(process_shutdown) = PROCESS_SHUTDOWN.get().map(Arc::clone) else {
        tracing::error!("terminal backend opened before shutdown handler installation");
        std::process::abort();
    };
    let shared = Shared {
        panes: Arc::new(Mutex::new(PaneGrids::new(usize::from(COLUMNS), usize::from(ROWS)))),
        attached: Arc::new(Mutex::new(HashSet::new())),
        status: Arc::new(Mutex::new(String::new())),
        generation: Arc::new(AtomicU64::new(0)),
        active_session: Arc::new(Mutex::new(None)),
        focused_size: Arc::new(Mutex::new(terminal_size)),
        tabs: Arc::new(Mutex::new(TabSessions::new())),
        connected: Arc::new(AtomicBool::new(false)),
        session_list_seen: Arc::new(AtomicBool::new(false)),
        initial_session: Arc::new(InitialSessionBootstrap::new(initial_session)),
        ai: Arc::new(Mutex::new(AiChrome::new(
            load_config().unwrap_or_default().terminal.ai_session.ai_states,
        ))),
        chrome_metadata: Arc::new(Mutex::new(ChromeMetadata::new())),
        share: Arc::new(Mutex::new(ShareChrome::new())),
        update: Arc::new(Mutex::new(UpdateState::default())),
        ci_runs: Arc::new(Mutex::new(CiRunBars::default())),
        ci_owner_controls,
        lifecycle: Arc::new(Mutex::new(WindowLifecycle::new())),
        process_shutdown,
        bells: Arc::new(Mutex::new(Vec::new())),
        ai_notices: Arc::new(Mutex::new(Vec::new())),
        notification_focus: Arc::new(Mutex::new(Vec::new())),
        deferred_grids: Arc::new(Mutex::new(Vec::new())),
        find: Arc::new(Mutex::new(FindResults::default())),
        lan: Arc::new(Mutex::new(LanChrome::new())),
        prompt_marks: Arc::new(Mutex::new(PromptMarks::new())),
        workspaces: Arc::new(Mutex::new(Vec::new())),
        server_topology: Arc::new(Mutex::new(None)),
        clipboard: Arc::new(Mutex::new(ClipboardBridge::default())),
        remote: Arc::new(Mutex::new(RemoteChrome::new())),
        beads_boards: Arc::new(Mutex::new(BeadsBoards::default())),
        beads_panels: Arc::new(Mutex::new(BeadsPanels::default())),
    };
    let (out_tx, out_rx) = outbound_channel();
    let (in_tx, in_rx) = inbound_channel();
    let sink = IpcSink::new(out_tx.clone());

    start_ipc_thread(IpcThread {
        shared: shared.clone(),
        sink: sink.clone(),
        out_tx,
        out_rx,
        in_tx,
        in_rx: Some(in_rx),
        claim,
        join_intent,
        fan_out,
    });

    (shared, sink)
}

fn launchd_command_exit() -> Option<std::process::ExitCode> {
    if let Some(active_slot) =
        scribe_common::macos_launchd::LaunchdSlot::registration_from_args(std::env::args())
    {
        let _ = active_slot;
        #[cfg(target_os = "macos")]
        return Some(
            match scribe_common::macos_launchd::activate_replacement(
                scribe_common::app::current_identity(),
                active_slot,
            ) {
                Ok(()) => std::process::ExitCode::SUCCESS,
                Err(error) => {
                    tracing::error!(
                        %error,
                        active_slot = active_slot.name(),
                        "launchd replacement registration failed"
                    );
                    std::process::ExitCode::FAILURE
                }
            },
        );

        #[cfg(not(target_os = "macos"))]
        return Some(std::process::ExitCode::FAILURE);
    }
    if let Some(active_slot) =
        scribe_common::macos_launchd::LaunchdSlot::inactive_unregistration_from_args(
            std::env::args(),
        )
    {
        let _ = active_slot;
        #[cfg(target_os = "macos")]
        return Some(
            match scribe_common::macos_launchd::unregister_inactive_slot(
                scribe_common::app::current_identity(),
                active_slot,
            ) {
                Ok(()) => std::process::ExitCode::SUCCESS,
                Err(error) => {
                    tracing::error!(
                        %error,
                        active_slot = active_slot.name(),
                        "inactive launchd unregister failed"
                    );
                    std::process::ExitCode::FAILURE
                }
            },
        );

        #[cfg(not(target_os = "macos"))]
        return Some(std::process::ExitCode::FAILURE);
    }
    if server_lifecycle::is_finish_update_restart(std::env::args()) {
        return Some(match server_lifecycle::finish_update_restart() {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(error) => {
                tracing::error!(%error, "deferred update restart failed");
                std::process::ExitCode::FAILURE
            }
        });
    }
    if let Some((old_server_pid, client_pids)) =
        server_lifecycle::client_relaunch_request(std::env::args())
    {
        let _ = (&old_server_pid, &client_pids);
        #[cfg(target_os = "macos")]
        return Some(
            match server_lifecycle::finish_client_relaunch(old_server_pid, &client_pids) {
                Ok(()) => std::process::ExitCode::SUCCESS,
                Err(error) => {
                    tracing::error!(%error, "post-update client relaunch failed");
                    std::process::ExitCode::FAILURE
                }
            },
        );

        #[cfg(not(target_os = "macos"))]
        return Some(std::process::ExitCode::FAILURE);
    }
    None
}

// @lat: [[client#Client#GPUI Window Lifecycle#Terminal client singleton]]
const fn terminal_singleton_required(exemptions: [bool; 4]) -> bool {
    !exemptions[0] && !exemptions[1] && !exemptions[2] && !exemptions[3]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalFocusTarget {
    Owner,
    RestoreChild { generation: ClientFocusGeneration, window_id: WindowId },
}

struct TerminalFocusBroker {
    sequence: u64,
    winner_sequence: u64,
    target: TerminalFocusTarget,
}

impl TerminalFocusBroker {
    const fn new() -> Self {
        Self { sequence: 0, winner_sequence: 0, target: TerminalFocusTarget::Owner }
    }

    fn record_owner(&mut self) {
        self.record(TerminalFocusTarget::Owner);
    }

    #[cfg(test)]
    fn record_restore_child(&mut self, generation: ClientFocusGeneration, window_id: WindowId) {
        self.record(TerminalFocusTarget::RestoreChild { generation, window_id });
    }

    fn record(&mut self, target: TerminalFocusTarget) {
        let receipt = self.reserve_receipt();
        self.record_at(receipt, target);
    }

    fn reserve_receipt(&mut self) -> u64 {
        self.sequence = self.sequence.saturating_add(1);
        self.sequence
    }

    fn record_restore_child_at(
        &mut self,
        receipt: u64,
        generation: ClientFocusGeneration,
        window_id: WindowId,
    ) {
        self.record_at(receipt, TerminalFocusTarget::RestoreChild { generation, window_id });
    }

    fn record_at(&mut self, receipt: u64, target: TerminalFocusTarget) {
        if receipt > self.winner_sequence {
            self.winner_sequence = receipt;
            self.target = target;
        }
    }

    const fn target(&self) -> TerminalFocusTarget {
        self.target
    }

    fn prune_restore_child(&mut self, generation: ClientFocusGeneration) -> bool {
        if !matches!(
            self.target,
            TerminalFocusTarget::RestoreChild { generation: current, .. } if current == generation
        ) {
            return false;
        }
        self.target = TerminalFocusTarget::Owner;
        true
    }

    #[cfg(test)]
    const fn sequence(&self) -> u64 {
        self.sequence
    }
}

#[derive(Clone)]
enum TerminalFocusReporter {
    Owner(Arc<Mutex<TerminalFocusBroker>>),
    RestoreChild(std::sync::mpsc::Sender<WindowId>),
}

fn report_terminal_activation(window_id: Option<WindowId>) {
    TERMINAL_FOCUS_REPORTER.with(|slot| match slot.borrow().as_ref() {
        Some(TerminalFocusReporter::Owner(broker)) => {
            if let Ok(mut broker) = broker.lock() {
                broker.record_owner();
            }
        }
        Some(TerminalFocusReporter::RestoreChild(publisher)) => {
            if let Some(window_id) = window_id
                && publisher.send(window_id).is_err()
            {
                tracing::debug!(%window_id, "restore-child focus publisher stopped");
            }
        }
        None => {}
    });
}

enum TerminalFocusAction {
    ActivateOwner,
    ActivateRestoreChild(std::sync::mpsc::SyncSender<bool>),
}

struct TerminalFocusListener {
    broker: Arc<Mutex<TerminalFocusBroker>>,
    focus_rx: tokio::sync::mpsc::UnboundedReceiver<TerminalFocusAction>,
}

fn start_terminal_focus_listener(
    listener: std::os::unix::net::UnixListener,
) -> Result<TerminalFocusListener, String> {
    listener
        .set_nonblocking(false)
        .map_err(|error| format!("failed to configure terminal singleton listener: {error}"))?;
    let (focus_tx, focus_rx) = unbounded_channel();
    let broker = Arc::new(Mutex::new(TerminalFocusBroker::new()));
    let listener_broker = Arc::clone(&broker);
    std::thread::Builder::new()
        .name("scribe-client-singleton".to_owned())
        .spawn(move || {
            loop {
                let stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(error) => {
                        tracing::warn!(%error, "terminal singleton listener stopped");
                        return;
                    }
                };
                handle_terminal_focus_connection(&stream, &listener_broker, &focus_tx);
            }
        })
        .map_err(|error| format!("failed to start terminal singleton listener: {error}"))?;
    Ok(TerminalFocusListener { broker, focus_rx })
}

fn handle_terminal_focus_connection(
    stream: &std::os::unix::net::UnixStream,
    broker: &Arc<Mutex<TerminalFocusBroker>>,
    focus_tx: &tokio::sync::mpsc::UnboundedSender<TerminalFocusAction>,
) {
    use scribe_client::settings::singleton::{self, TerminalFocusCommand};

    let command = singleton::read_terminal_focus_command(stream)
        .inspect_err(|error| tracing::warn!(%error, "terminal singleton command was rejected"));
    let Ok(command) = command else {
        return;
    };
    match &command {
        TerminalFocusCommand::Focus { .. } if singleton::verify_peer_uid(stream) => {
            route_terminal_focus(broker, focus_tx);
        }
        TerminalFocusCommand::AnnounceActivation { generation, window_id, .. } => {
            let receipt = match broker.lock() {
                Ok(mut registry) => registry.reserve_receipt(),
                Err(_) => return,
            };
            if let Err(error) = singleton::authenticate_activation_announcement(stream, &command) {
                tracing::warn!(%error, "restore-child activation was rejected");
                return;
            }
            if let Ok(mut registry) = broker.lock() {
                registry.record_restore_child_at(receipt, *generation, *window_id);
            }
        }
        TerminalFocusCommand::Focus { .. } => {}
    }
}

fn route_terminal_focus(
    broker: &Arc<Mutex<TerminalFocusBroker>>,
    focus_tx: &tokio::sync::mpsc::UnboundedSender<TerminalFocusAction>,
) {
    use scribe_client::settings::singleton::{self, FocusEndpointResult};

    let target = broker.lock().map_or(TerminalFocusTarget::Owner, |broker| broker.target());
    if let TerminalFocusTarget::RestoreChild { generation, window_id } = target {
        match singleton::request_focus_endpoint(generation) {
            Ok(FocusEndpointResult::Activated { .. }) => return,
            Ok(result) => tracing::debug!(?result, %window_id, "restore-child focus fell back"),
            Err(error) => {
                tracing::debug!(%error, %window_id, "restore-child focus target was unavailable");
            }
        }
        if let Ok(mut broker) = broker.lock() {
            broker.prune_restore_child(generation);
        }
    }
    drop(focus_tx.send(TerminalFocusAction::ActivateOwner));
}

async fn drive_terminal_focus(
    mut focus_rx: tokio::sync::mpsc::UnboundedReceiver<TerminalFocusAction>,
    app: &mut AsyncApp,
) {
    while let Some(action) = focus_rx.recv().await {
        let target = RECENT_TERMINAL_WINDOW.with(|recent| *recent.borrow());
        let activated = target.is_some_and(|window_handle| {
            window_handle.update(app, |_, window, _| window.activate_window()).is_ok()
        });
        if !activated {
            RECENT_TERMINAL_WINDOW.with(|recent| recent.replace(None));
        }
        if let TerminalFocusAction::ActivateRestoreChild(response) = action
            && response.send(activated).is_err()
        {
            tracing::debug!("restore-child focus requester stopped");
        }
    }
}

fn dispatch_focus_endpoint_request(
    request: &scribe_client::settings::singleton::FocusEndpointRequest,
    generation: ClientFocusGeneration,
    activate: impl FnOnce() -> bool,
) -> scribe_client::settings::singleton::FocusEndpointResult {
    use scribe_client::settings::singleton::{
        FocusEndpointRejection, FocusEndpointRequest, FocusEndpointResult,
        validate_focus_endpoint_request,
    };

    if validate_focus_endpoint_request(request, generation).is_err() {
        return FocusEndpointResult::Rejected {
            reason: FocusEndpointRejection::GenerationMismatch,
        };
    }
    match request {
        FocusEndpointRequest::Probe { .. } => FocusEndpointResult::Alive { generation },
        FocusEndpointRequest::Activate { .. } if activate() => {
            FocusEndpointResult::Activated { generation }
        }
        FocusEndpointRequest::Activate { .. } => {
            FocusEndpointResult::Rejected { reason: FocusEndpointRejection::UnavailableWindow }
        }
    }
}

struct RestoreChildFocusRuntime {
    shutdown: Arc<AtomicBool>,
    listener: Option<std::thread::JoinHandle<()>>,
    publisher: Option<std::thread::JoinHandle<()>>,
}

impl Drop for RestoreChildFocusRuntime {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(listener) = self.listener.take() {
            drop(listener.join());
        }
        if let Some(publisher) = self.publisher.take() {
            drop(publisher.join());
        }
    }
}

struct RestoreChildFocusStartup {
    runtime: RestoreChildFocusRuntime,
    publisher: std::sync::mpsc::Sender<WindowId>,
    focus_rx: tokio::sync::mpsc::UnboundedReceiver<TerminalFocusAction>,
}

fn start_restore_child_focus() -> Result<RestoreChildFocusStartup, String> {
    use scribe_client::settings::singleton::BoundFocusEndpoint;

    let generation = ClientFocusGeneration::new();
    let endpoint = BoundFocusEndpoint::bind(generation).map_err(|error| error.to_string())?;
    endpoint
        .listener()
        .set_nonblocking(true)
        .map_err(|error| format!("failed to configure restore-child focus endpoint: {error}"))?;
    let shutdown = Arc::new(AtomicBool::new(false));
    let listener_shutdown = Arc::clone(&shutdown);
    let (focus_tx, focus_rx) = unbounded_channel();
    let listener = std::thread::Builder::new()
        .name("scribe-focus-endpoint".to_owned())
        .spawn(move || {
            while !listener_shutdown.load(Ordering::Acquire) {
                let stream = match endpoint.listener().accept() {
                    Ok((stream, _)) => stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => {
                        tracing::warn!(%error, "restore-child focus endpoint stopped");
                        return;
                    }
                };
                serve_restore_child_focus_connection(stream, generation, &focus_tx);
            }
        })
        .map_err(|error| format!("failed to start restore-child focus endpoint: {error}"))?;

    let (activation_tx, activation_rx) = std::sync::mpsc::channel();
    let publisher_shutdown = Arc::clone(&shutdown);
    let publisher = match std::thread::Builder::new()
        .name("scribe-focus-publisher".to_owned())
        .spawn(move || {
            publish_restore_child_activations(&activation_rx, &publisher_shutdown, generation);
        }) {
        Ok(publisher) => publisher,
        Err(error) => {
            shutdown.store(true, Ordering::Release);
            drop(listener.join());
            return Err(format!("failed to start restore-child focus publisher: {error}"));
        }
    };
    Ok(RestoreChildFocusStartup {
        runtime: RestoreChildFocusRuntime {
            shutdown,
            listener: Some(listener),
            publisher: Some(publisher),
        },
        publisher: activation_tx,
        focus_rx,
    })
}

fn publish_restore_child_activations(
    activation_rx: &std::sync::mpsc::Receiver<WindowId>,
    shutdown: &AtomicBool,
    generation: ClientFocusGeneration,
) {
    while !shutdown.load(Ordering::Acquire) {
        match activation_rx.recv_timeout(Duration::from_millis(10)) {
            Ok(window_id) => publish_restore_child_activation(generation, window_id),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn publish_restore_child_activation(generation: ClientFocusGeneration, window_id: WindowId) {
    if let Err(error) =
        scribe_client::settings::singleton::announce_activation(generation, window_id)
    {
        tracing::debug!(%error, %window_id, "restore-child activation dropped");
    }
}

fn serve_restore_child_focus_connection(
    mut stream: std::os::unix::net::UnixStream,
    generation: ClientFocusGeneration,
    focus_tx: &tokio::sync::mpsc::UnboundedSender<TerminalFocusAction>,
) {
    use scribe_client::settings::singleton::{
        self, ExpectedClientRole, FocusEndpointRejection, FocusEndpointResult,
    };

    if singleton::verify_focus_peer(&stream, ExpectedClientRole::SingletonOwner).is_err() {
        drop(singleton::write_focus_endpoint_result(
            &mut stream,
            &FocusEndpointResult::Rejected { reason: FocusEndpointRejection::Unauthorized },
        ));
        return;
    }
    let Ok(request) = singleton::read_focus_endpoint_request(&stream) else {
        drop(singleton::write_focus_endpoint_result(
            &mut stream,
            &FocusEndpointResult::Rejected { reason: FocusEndpointRejection::Malformed },
        ));
        return;
    };
    let result = dispatch_focus_endpoint_request(&request, generation, || {
        let (response_tx, response_rx) = std::sync::mpsc::sync_channel(1);
        focus_tx.send(TerminalFocusAction::ActivateRestoreChild(response_tx)).is_ok()
            && response_rx.recv_timeout(singleton::FOCUS_IO_TIMEOUT).unwrap_or(false)
    });
    drop(singleton::write_focus_endpoint_result(&mut stream, &result));
}

enum TerminalSingletonStartup {
    Exempt,
    AlreadyRunning,
    Primary {
        socket_path: PathBuf,
        broker: Arc<Mutex<TerminalFocusBroker>>,
        focus_rx: tokio::sync::mpsc::UnboundedReceiver<TerminalFocusAction>,
    },
}

fn prepare_terminal_singleton(required: bool) -> Result<TerminalSingletonStartup, String> {
    use scribe_client::settings::singleton::{self, SingletonResult};

    if !required {
        return Ok(TerminalSingletonStartup::Exempt);
    }
    match singleton::acquire_terminal()? {
        SingletonResult::AlreadyRunning => Ok(TerminalSingletonStartup::AlreadyRunning),
        SingletonResult::Primary { listener, socket_path } => {
            let focus = start_terminal_focus_listener(listener).inspect_err(|_| {
                singleton::cleanup_socket(&socket_path);
            })?;
            Ok(TerminalSingletonStartup::Primary {
                socket_path,
                broker: focus.broker,
                focus_rx: focus.focus_rx,
            })
        }
    }
}

enum TerminalFocusSetup {
    Exempt,
    Owner {
        broker: Arc<Mutex<TerminalFocusBroker>>,
        focus_rx: tokio::sync::mpsc::UnboundedReceiver<TerminalFocusAction>,
    },
    RestoreChild,
}

enum TerminalFocusLaunch {
    AlreadyRunning,
    Start { socket_path: Option<PathBuf>, setup: TerminalFocusSetup },
}

fn prepare_terminal_focus_launch(restore_child: bool) -> Result<TerminalFocusLaunch, String> {
    let explicit_join = std::env::var_os(scribe_client::share_join::JOIN_WINDOW_ENV).is_some();
    let remote_dial = std::env::var_os(remote_handshake::REMOTE_DIAL_ENV).is_some();
    let lan_dial = std::env::var_os(remote_handshake::LAN_DIAL_ENV).is_some();
    let required =
        terminal_singleton_required([restore_child, explicit_join, remote_dial, lan_dial]);
    match prepare_terminal_singleton(required)? {
        TerminalSingletonStartup::Exempt => Ok(TerminalFocusLaunch::Start {
            socket_path: None,
            setup: if restore_child && !explicit_join && !remote_dial && !lan_dial {
                TerminalFocusSetup::RestoreChild
            } else {
                TerminalFocusSetup::Exempt
            },
        }),
        TerminalSingletonStartup::AlreadyRunning => Ok(TerminalFocusLaunch::AlreadyRunning),
        TerminalSingletonStartup::Primary { socket_path, broker, focus_rx } => {
            Ok(TerminalFocusLaunch::Start {
                socket_path: Some(socket_path),
                setup: TerminalFocusSetup::Owner { broker, focus_rx },
            })
        }
    }
}

fn run_terminal_app(
    shared: Shared,
    sink: IpcSink,
    terminal_size: TerminalSize,
    cold_start: ColdStart,
    focus_setup: TerminalFocusSetup,
) {
    let restore_child_runtime = Arc::new(Mutex::new(None));
    let runtime_slot = Arc::clone(&restore_child_runtime);
    application().run(move |cx: &mut App| {
        gpui_tokio::init(cx);
        scribe_client::fonts::register_embedded_fonts(cx);
        let animations = load_config().map_or(true, |config| config.appearance.animations);
        AnimationSettings::resolve(animations).apply_to_app(cx);
        app_shortcuts::register(cx);
        let focus_rx = match focus_setup {
            TerminalFocusSetup::Exempt => None,
            TerminalFocusSetup::Owner { broker, focus_rx } => {
                TERMINAL_FOCUS_REPORTER.with(|slot| {
                    slot.replace(Some(TerminalFocusReporter::Owner(broker)));
                });
                Some(focus_rx)
            }
            TerminalFocusSetup::RestoreChild => match start_restore_child_focus() {
                Ok(focus) => {
                    TERMINAL_FOCUS_REPORTER.with(|slot| {
                        slot.replace(Some(TerminalFocusReporter::RestoreChild(focus.publisher)));
                    });
                    if let Ok(mut slot) = runtime_slot.lock() {
                        *slot = Some(focus.runtime);
                    }
                    Some(focus.focus_rx)
                }
                Err(error) => {
                    tracing::warn!(%error, "restore-child focus transport is unavailable");
                    None
                }
            },
        };
        if let Some(terminal_window) = open_window(cx, &shared, &sink, terminal_size, cold_start) {
            RECENT_TERMINAL_WINDOW.with(|recent| recent.replace(Some(terminal_window)));
            cx.on_action(move |_: &Quit, cx| {
                defer_terminal_window_close(terminal_window, cx);
            });
        }
        if let Some(focus_rx) = focus_rx {
            cx.spawn(async move |app| drive_terminal_focus(focus_rx, app).await).detach();
        }
        cx.activate(true);
    });
    if let Ok(mut slot) = restore_child_runtime.lock() {
        drop(slot.take());
    }
}

fn main() -> std::process::ExitCode {
    PROCESS_START.get_or_init(Instant::now);
    init_tracing();
    if let Some(exit) = launchd_command_exit() {
        return exit;
    }
    if std::env::args().skip(1).any(|arg| arg == "--vulkan-probe") {
        if let Err(error) = probe_vulkan() {
            tracing::error!(%error, "Scribe Vulkan probe failed");
            return std::process::ExitCode::FAILURE;
        }
        return std::process::ExitCode::SUCCESS;
    }
    if std::env::args().skip(1).any(|arg| arg == "--gpui-image-spike") {
        scribe_client::gpui_image_spike::run();
        return std::process::ExitCode::SUCCESS;
    }
    if std::env::args().skip(1).any(|arg| arg == "--terminal-image-renderer-probe") {
        terminal_image_renderer_probe::run();
        return std::process::ExitCode::SUCCESS;
    }
    // Arm the perf rig's runtime probe before anything can paint or type; it
    // stays inert unless `SCRIBE_PERF_PROBE` names a report path.
    scribe_common::perf_probe::init_from_env();
    // @lat: [[client#IPC Client#Server Lifecycle]]
    scribe_client::hook_setup::repair_ai_hooks_on_startup();

    // `scribe-client --settings` opens (or focuses) the settings window instead
    // of the terminal shell. The singleton absorbs the retired settings app's
    // `settings.lock`/`settings.sock`: a second launch hands focus to the
    // running window and exits here.
    if std::env::args().skip(1).any(|arg| arg == "--settings") {
        run_settings();
        return std::process::ExitCode::SUCCESS;
    }

    let process_shutdown = match ProcessShutdown::install() {
        Ok(shutdown) => shutdown,
        Err(error) => {
            tracing::error!(%error, "cannot install terminal shutdown handler");
            return std::process::ExitCode::FAILURE;
        }
    };
    if PROCESS_SHUTDOWN.set(process_shutdown).is_err() {
        tracing::error!("terminal shutdown handler was installed twice");
        return std::process::ExitCode::FAILURE;
    }

    let restore_child = restore_replay::is_restore_child(std::env::args());
    let (terminal_socket_path, focus_setup) = match prepare_terminal_focus_launch(restore_child) {
        Ok(TerminalFocusLaunch::AlreadyRunning) => {
            tracing::info!("terminal client already running; sent focus and exiting");
            return std::process::ExitCode::SUCCESS;
        }
        Ok(TerminalFocusLaunch::Start { socket_path, setup }) => (socket_path, setup),
        Err(error) => {
            tracing::error!(%error, "failed to acquire terminal singleton");
            return std::process::ExitCode::FAILURE;
        }
    };
    let join_window = scribe_client::share_join::join_window_from_env();

    // Claimed before the backend connects: the claimed snapshot's geometry is
    // what the window is opened at, and it is only findable under that
    // snapshot's own (pre-crash) window id.
    let cold_start = ColdStart::resolve();
    let terminal_size = default_terminal_size();
    // `SCRIBE_JOIN_WINDOW` (unset for a user-launched client) names a window
    // another local process already holds: the server resolves that non-takeover
    // claim as an additive share join under any non-`single_controller` sharing
    // mode, so this client renders and types into the SAME panes instead of
    // opening an empty window of its own — and must not create a shell there.
    let bootstrap_initial_session = cold_start.snapshot.is_none() && join_window.is_none();
    // The claimed snapshot names the window this process is resuming. Claiming
    // it back is what makes the resume line up: a server that kept the window
    // hands back *its* sessions rather than whichever window it happened to
    // offer, and a server that lost them assigns the id verbatim, so the replay
    // and the geometry this window is opening at belong to the same window. With
    // nothing claimed there is no id to name, and the unnamed `Hello` asks the
    // server for any window whose sessions outlived their client.
    let (shared, sink) = start_window_backend(
        terminal_size,
        WindowBackend {
            claim: join_window.or_else(|| cold_start.snapshot.as_ref().map(|snap| snap.window_id)),
            join_intent: if join_window.is_some() {
                LocalJoinIntent::Join
            } else {
                LocalJoinIntent::Plain
            },
            initial_session: bootstrap_initial_session,
            // The bootstrap reopens the server's other windows; a
            // `--restore-child` is already the other half of the client-side
            // cold-restart fan-out.
            fan_out: !restore_child,
        },
    );

    run_terminal_app(shared, sink, terminal_size, cold_start, focus_setup);
    if let Some(socket_path) = terminal_socket_path {
        scribe_client::settings::singleton::cleanup_socket(&socket_path);
    }
    std::process::ExitCode::SUCCESS
}

/// Request the terminal close after GPUI finishes dispatching the Quit action.
fn defer_terminal_window_close(terminal_window: WindowHandle<TerminalView>, cx: &mut App) {
    // GPUI removes the active window from its window table while dispatching an
    // action through it. Updating the handle synchronously would therefore
    // fail; defer until dispatch has returned the window to the table.
    cx.defer(move |cx| {
        if terminal_window
            .update(cx, |view, _window, ctx| {
                view.request_window_close(ctx);
            })
            .is_err()
        {
            tracing::warn!("quit shortcut ignored: terminal window is unavailable");
        }
    });
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

    let (listener, socket_path) = match singleton::acquire(None) {
        Ok(SingletonResult::Primary { listener, socket_path }) => (listener, socket_path),
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
        // Sidebar icons use the same embedded glyph set as the terminal grid.
        scribe_client::fonts::register_embedded_fonts(cx);
        let animations = load_config().map_or(true, |config| config.appearance.animations);
        AnimationSettings::resolve(animations).apply_to_app(cx);
        app_shortcuts::register(cx);
        cx.on_action(|_: &Quit, cx| cx.quit());
        // The handle is only useful to a caller that can be asked twice; this
        // process exists to show one settings window and exit with it.
        open_settings_window(cx);
        cx.activate(true);
    });

    // Hold the singleton guards for the window's lifetime, then clean up.
    singleton::cleanup_socket(&socket_path);
    drop(listener);
}

fn open_window(
    cx: &mut App,
    shared: &Shared,
    sink: &IpcSink,
    terminal_size: TerminalSize,
    cold_start: ColdStart,
) -> Option<WindowHandle<TerminalView>> {
    let bounds = Bounds::centered(None, startup_window_size(cx), cx);
    // A cold-restart window knows its record before it opens, so its pinned
    // boards are seeded here; a window the server assigns an id to instead
    // picks them up in `adopt_assigned_geometry`.
    if let Some(geometry) = cold_start.geometry.as_ref()
        && let Ok(mut boards) = shared.beads_boards.lock()
    {
        boards.restore_pins(geometry.beads_pinned.iter().copied());
    }
    // Restored geometry wins over the grid-derived startup size. Non-X11
    // platforms receive its bounds and state in GPUI's creation options; X11
    // keeps the saved restore rect but strips GPUI's state toggle below, then
    // Scribe asserts that state after the map and before any application frame.
    let window_bounds = cold_start
        .geometry
        .as_ref()
        .map_or(WindowBounds::Windowed(bounds), |geom| window_bounds_for(geom, bounds));
    // GPUI's X11 `WindowBounds::Maximized`/`Fullscreen` path sends a toggle on
    // its own X connection. Our idempotent ADD uses another connection after
    // the map, so the two requests have no ordering and a late toggle can undo
    // the restore. Open at the saved restore rect on X11 and let Scribe's ADD be
    // the sole state transition. Wayland keeps GPUI's native pre-map request.
    #[cfg(target_os = "linux")]
    let window_bounds = if gpui::guess_compositor() == "X11" {
        x11_creation_bounds(window_bounds)
    } else {
        window_bounds
    };
    if let Some(geom) = cold_start.geometry.as_ref() {
        tracing::info!(
            width = geom.width,
            height = geom.height,
            state = ?geom.state,
            restore_state = ?geom.restore_state,
            zoom = geom.zoom,
            "restoring persisted window geometry"
        );
    }
    let restored = cold_start.snapshot;
    let restore_siblings = cold_start.siblings;
    let restore_geometry = cold_start.geometry;
    let startup_state = restore_geometry
        .as_ref()
        .map(WindowGeometry::effective_state)
        .filter(|state| matches!(state, WindowState::Maximized | WindowState::Fullscreen));
    let shared = shared.clone();
    let sink = sink.clone();
    // Everything between here and the root-view builder below happens inside
    // gpui: window creation, wgpu adapter enumeration, device creation and
    // surface configure. Timing it separates the platform GPU bring-up floor
    // from Scribe's own startup work for the perf gate.
    let bringup_start = Instant::now();
    match cx.open_window(
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
            // Earliest Scribe-owned point: GPUI has mapped the platform window,
            // but no root view exists and no application frame can paint yet.
            // The render-path assertion remains for records adopted from
            // `Welcome`, which were unknowable when their window was created.
            if let Some(state) = startup_state {
                monitor::assert_window_state(window, state);
            }
            cx.new(|cx| {
                TerminalView::new(
                    shared,
                    sink,
                    WindowSeed { terminal_size, restored, restore_siblings, restore_geometry },
                    window,
                    cx,
                )
            })
        },
    ) {
        Ok(handle) => Some(handle),
        Err(error) => {
            tracing::error!(%error, "failed to open GPUI window");
            None
        }
    }
}

/// Strip GPUI's stateful X11 toggle from restored creation bounds.
#[cfg(target_os = "linux")]
fn x11_creation_bounds(bounds: WindowBounds) -> WindowBounds {
    match bounds {
        WindowBounds::Maximized(bounds) | WindowBounds::Fullscreen(bounds) => {
            WindowBounds::Windowed(bounds)
        }
        WindowBounds::Windowed(bounds) => WindowBounds::Windowed(bounds),
    }
}

/// Everything the background IPC thread owns for one connection.
struct IpcThread {
    shared: Shared,
    sink: IpcSink,
    out_tx: OutboundSender,
    out_rx: OutboundReceiver,
    in_tx: InboundSender,
    in_rx: Option<InboundReceiver>,
    /// The window this backend was told to claim before it ever handshook: the
    /// resumed window's id for the bootstrap, a freshly minted one for a
    /// deliberate new window, the id the server named in `other_windows` for a
    /// restored sibling, or `SCRIBE_JOIN_WINDOW`'s for a share join. `None` asks
    /// the server to hand back any window whose sessions outlived their client.
    claim: Option<WindowId>,
    /// Whether this is the explicit `SCRIBE_JOIN_WINDOW` share-join path.
    join_intent: LocalJoinIntent,
    /// Whether this connection may act on `Welcome`'s `other_windows`. Set for
    /// the process bootstrap and consumed by its first handshake: a redial sees
    /// the same list while those windows' own processes are reconnecting too,
    /// and reopening one would race its real client for the claim.
    fan_out: bool,
}

impl IpcThread {
    /// The window this connection's `Hello` claims.
    ///
    /// Once `Welcome` has named this window the id is binding: every redial
    /// claims it back, so a hot upgrade — which drops every stream and lets all
    /// windows redial at once — cannot shuffle two windows' session sets
    /// through the server's "adopt any unconnected window" path.
    fn window_claim(&self) -> Option<WindowId> {
        self.shared.lifecycle.lock().ok().and_then(|lifecycle| lifecycle.window_id()).or(self.claim)
    }
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

/// Why the writer tore a stream that had not failed on its own.
///
/// Named once because it is both the writer's error and the phrasing the status
/// strip shows: the cap is only ever reached behind a server that has stopped
/// reading, and the user needs to be told that rather than left typing into a
/// queue that is going nowhere.
const OUTBOUND_TEAR_REASON: &str = "outbound queue full; the server is not draining";

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
        ctx.sink.clone(),
        Arc::clone(&ctx.shared.panes),
        Arc::clone(&ctx.shared.generation),
        Arc::clone(&ctx.shared.prompt_marks),
    );

    loop {
        let result = run_connection(&mut ctx).await;
        // A killed window's backend is done: its sessions are destroyed and its
        // window is gone from the workspace manager, so a redial would claim
        // (see [`IpcThread::window_claim`]) a window id the server no longer
        // has, resurrecting it as a ghost. The process outlives this thread
        // because it may still be hosting other windows.
        if ctx.shared.lifecycle.lock().is_ok_and(|lifecycle| lifecycle.window_closed()) {
            tracing::info!("window closed — its IPC backend is shutting down");
            return;
        }
        // Reader and writer failures also end connections that were fully live,
        // so the Result alone cannot distinguish them from failed dial attempts.
        // Consume the shared connection marker before choosing the next delay:
        // every new outage starts at the short initial retry interval.
        if ctx.shared.connected.swap(false, Ordering::AcqRel) {
            reconnect_delay = INITIAL_RECONNECT_DELAY;
        }

        match result {
            Ok(()) => tracing::info!("server connection closed"),
            Err(error) => {
                // Logged as well as published: the status line is one line wide
                // and the reason a connect failed — a stale socket, a refused
                // autostart, a rejected dial — is the first thing anyone
                // diagnosing a dead window needs, long after the window is gone.
                tracing::warn!(%error, "server connection failed");
                if !retry_local {
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

        // A queue still at its cap outranks the generic "connection lost" line:
        // the user is typing into a window that is refusing input, and that is
        // the fact that has to be on screen while the redial backs off.
        let reason = if ctx.out_rx.is_refusing() {
            format!(
                "input refused: {OUTBOUND_TEAR_REASON} ({OUTBOUND_QUEUE_FRAMES} frames queued); \
                 retrying in {} ms",
                reconnect_delay.as_millis()
            )
        } else {
            format!("server connection lost; retrying in {} ms", reconnect_delay.as_millis())
        };
        set_status(&ctx.shared.status, &ctx.shared.generation, reason);
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
    let connection = match server_lifecycle::connect_or_start_server(&socket_path).await {
        Ok(connection) => connection,
        Err(error) => {
            if error.cold_restart_required
                && let Ok(mut update) = ctx.shared.update.lock()
            {
                update.on_progress(UpdateProgressState::CompletedRestartRequired {
                    version: env!("CARGO_PKG_VERSION").to_owned(),
                });
            }
            return Err(error.to_string());
        }
    };
    let stream = connection.stream;
    if connection.cold_restart_required
        && let Ok(mut update) = ctx.shared.update.lock()
    {
        update.on_progress(UpdateProgressState::CompletedRestartRequired {
            version: env!("CARGO_PKG_VERSION").to_owned(),
        });
    }
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
    } else {
        set_status(&ctx.shared.status, &ctx.shared.generation, String::new());
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
    publish_remote_status(
        &remote,
        &status,
        &generation,
        outcome != RemoteConnectOutcome::Accepted,
        |chrome| {
            chrome.settle_dial(outcome);
        },
    );
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

/// Apply `mutate` to the tailnet chrome, publishing only warning/error copy.
///
/// The dial path runs before any [`ReaderCtx`] exists, so it holds the three
/// shared handles directly rather than going through [`update_remote_chrome`].
fn publish_remote_status(
    remote: &Arc<Mutex<RemoteChrome>>,
    status: &Arc<Mutex<String>>,
    generation: &Arc<AtomicU64>,
    publish: bool,
    mutate: impl FnOnce(&mut RemoteChrome),
) {
    let Ok(mut guard) = remote.lock() else {
        tracing::warn!("remote chrome mutex poisoned; dropping dial update");
        return;
    };
    mutate(&mut guard);
    let line = publish.then(|| guard.status_line()).flatten();
    drop(guard);
    set_status(status, generation, line.unwrap_or_default());
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
            true,
            LanChrome::awaiting_approval,
        );
    })
    .await;

    publish_lan_status(
        &lan,
        &status,
        &generation,
        outcome != LanConnectOutcome::Accepted,
        |chrome| chrome.settle_dial(outcome),
    );
    if outcome != LanConnectOutcome::Accepted {
        let (peer_host, peer_port) = dialer.target();
        return Err(format!("LAN dial to {peer_host}:{peer_port} was not accepted"));
    }

    // Approved: the encrypted link now behaves exactly like the local socket.
    ctx.shared.connected.store(true, Ordering::Release);
    let (reader, writer) = tokio::io::split(stream);
    serve_connection(ctx, reader, writer, Transport::Lan).await
}

/// Apply `mutate` to the LAN chrome, publishing only warning/error copy.
///
/// The dial path runs before any [`ReaderCtx`] exists, so it holds the three
/// shared handles directly rather than going through [`update_lan_chrome`].
fn publish_lan_status(
    lan: &Arc<Mutex<LanChrome>>,
    status: &Arc<Mutex<String>>,
    generation: &Arc<AtomicU64>,
    publish: bool,
    mutate: impl FnOnce(&mut LanChrome),
) {
    let Ok(mut guard) = lan.lock() else {
        tracing::warn!("LAN chrome mutex poisoned; dropping dial update");
        return;
    };
    mutate(&mut guard);
    let line = publish.then(|| guard.status_line()).flatten();
    drop(guard);
    set_status(status, generation, line.unwrap_or_default());
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
    // A local or LAN connection claims the window this backend was started for
    // (see [`IpcThread::window_claim`]) — `SCRIBE_JOIN_WINDOW`'s share join, a
    // deliberate new window's freshly minted id, a restored sibling's id, or the
    // id a previous `Welcome` already assigned this connection. A tailnet dial
    // instead carries the claim the connect picker made, including the
    // explicit-attach takeover that may displace a connected controller.
    let (claim_window, takeover) = match &transport {
        Transport::Local(_) | Transport::Lan => (ctx.window_claim(), false),
        Transport::Remote { window_id, takeover } => (*window_id, *takeover),
    };
    if let Some(window_id) = claim_window {
        tracing::info!(%window_id, takeover, "claiming an existing window");
    }
    // One-shot: the bootstrap's first handshake reopens the server's other
    // windows, and no later handshake on this or any other connection does.
    let fan_out_other_windows = std::mem::take(&mut ctx.fan_out);
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
            join_window: matches!(&transport, Transport::Local(_))
                && matches!(ctx.join_intent, LocalJoinIntent::Join),
            // Spec 020: terminal images are on by default, so this announces
            // the complete v1 renderer unless the user set
            // `terminal.images.enabled = false`. A capable viewer latches the
            // sessions it attaches to, which is what turns on parsing,
            // discovery replies, and retained image state for every
            // application running in them.
            // @lat: [[terminal-images#Terminal Images#Pinned Application Corpus]]
            terminal_images: scribe_common::terminal_images::advertised_capabilities(),
            ci_run_bar: true,
        },
    )
    .await
    .map_err(|error| error.to_string())?;
    write_message(&mut writer, &ClientMessage::ListSessions)
        .await
        .map_err(|error| error.to_string())?;

    // A tear belongs to the stream it was refused on. Frames refused while no
    // stream was alive have nothing to tear, and carrying that request into the
    // fresh connection would tear it before it wrote a single queued frame —
    // the queue would stay at its cap forever. The backlog itself is kept: a
    // `CreateSession` queued before the stream died is still a session the user
    // asked for, so the redial sends it rather than pruning it.
    let _ = ctx.out_rx.take_tear_request();

    let reader_ctx = reader_ctx(ctx, fan_out_other_windows);
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
fn reader_ctx(ctx: &IpcThread, fan_out_other_windows: bool) -> ReaderCtx {
    ReaderCtx {
        fan_out_other_windows,
        panes: Arc::clone(&ctx.shared.panes),
        attached: Arc::clone(&ctx.shared.attached),
        status: Arc::clone(&ctx.shared.status),
        generation: Arc::clone(&ctx.shared.generation),
        active_session: Arc::clone(&ctx.shared.active_session),
        focused_size: Arc::clone(&ctx.shared.focused_size),
        ai: Arc::clone(&ctx.shared.ai),
        chrome_metadata: Arc::clone(&ctx.shared.chrome_metadata),
        tabs: Arc::clone(&ctx.shared.tabs),
        session_list_seen: Arc::clone(&ctx.shared.session_list_seen),
        initial_session: Arc::clone(&ctx.shared.initial_session),
        share: Arc::clone(&ctx.shared.share),
        update: Arc::clone(&ctx.shared.update),
        ci_runs: Arc::clone(&ctx.shared.ci_runs),
        lifecycle: Arc::clone(&ctx.shared.lifecycle),
        bells: Arc::clone(&ctx.shared.bells),
        ai_notices: Arc::clone(&ctx.shared.ai_notices),
        deferred_grids: Arc::clone(&ctx.shared.deferred_grids),
        find: Arc::clone(&ctx.shared.find),
        lan: Arc::clone(&ctx.shared.lan),
        prompt_marks: Arc::clone(&ctx.shared.prompt_marks),
        workspaces: Arc::clone(&ctx.shared.workspaces),
        server_topology: Arc::clone(&ctx.shared.server_topology),
        clipboard: Arc::clone(&ctx.shared.clipboard),
        remote: Arc::clone(&ctx.shared.remote),
        beads_boards: Arc::clone(&ctx.shared.beads_boards),
        beads_panels: Arc::clone(&ctx.shared.beads_panels),
        out_tx: ctx.out_tx.clone(),
        in_tx: ctx.in_tx.clone(),
        sink: ctx.sink.clone(),
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

/// Drains the bounded IPC-writer queue to the socket in FIFO order, tearing the
/// connection when the queue reaches [`OUTBOUND_QUEUE_FRAMES`].
///
/// The frame in hand is requeued on every exit that is not a successful write,
/// so neither a dead socket nor a deliberate tear costs the user a keystroke;
/// the queue is never pruned across the redial either, because a `CreateSession`
/// queued before the stream died is still a session the user asked for.
async fn run_writer<W>(mut writer: W, out_rx: &mut OutboundReceiver) -> Result<(), String>
where
    W: AsyncWriteExt + Unpin,
{
    let tear = out_rx.tear_watch();
    loop {
        // A refusal that lands between two writes still tears this stream; the
        // race with an in-progress write is handled inside `write_or_tear`.
        if out_rx.take_tear_request() {
            return Err(OUTBOUND_TEAR_REASON.to_owned());
        }
        let Some(message) = out_rx.recv().await else {
            return Err("IPC writer channel closed".to_owned());
        };
        match write_or_tear(&mut writer, &message, &tear).await {
            WriteOutcome::Wrote => {}
            WriteOutcome::Failed(error) => {
                out_rx.requeue(message);
                return Err(format!("IPC writer stopped: {error}"));
            }
            WriteOutcome::Torn => {
                out_rx.requeue(message);
                return Err(OUTBOUND_TEAR_REASON.to_owned());
            }
        }
    }
}

/// What applying one batch left behind on the panes it touched.
struct BatchOutcome {
    /// Repaints the batch owes the redraw generation.
    redraws: usize,
    /// A synchronized update is open somewhere and needs the expiry task.
    sync_armed: bool,
    /// A pane still has committed bursts queued, so the pacer has work.
    frames_queued: bool,
}

/// Spawns the coalescing drain with synchronized-frame queueing in front of
/// `write_output`. A first task drains the inbound channel with Zed's
/// 4 ms / 100-event coalescing, capped at a megabyte of payload per batch
/// ([`run_drain`]), splits each pane's bytes into committed `CSI ? 2026` bursts
/// via that pane's [`PaneStream`], and presents one burst per redraw so no frame
/// tears across IPC message boundaries and no run of committed frames collapses
/// into a single repaint. A second task waits on the nearest raw-frame or parser
/// sync deadline and flushes a 150 ms-expired update whose terminating
/// `CSI ? 2026 l` never arrived. A third — [`run_frame_pacer`] — presents the
/// bursts pacing held back, because a pane whose last batch queued more than one
/// committed frame must not wait for the next batch to show the rest.
///
/// The `panes` registry lock is taken once per batch, to resolve the batch's
/// panes into handles, and released before a single byte is parsed. Everything
/// after that runs under the individual pane's own lock, so a firehose on one
/// pane blocks neither the renderer nor any other pane.
///
/// `sink` is the drain's own outbound handle: the inbound queue is bounded, so
/// whatever it had to drop is repaired by a `RequestSnapshot` per affected pane
/// once the drain catches up.
fn spawn_drain(
    in_rx: InboundReceiver,
    sink: IpcSink,
    panes: Arc<Mutex<PaneGrids>>,
    generation: Arc<AtomicU64>,
    prompt_marks: Arc<Mutex<PromptMarks>>,
) {
    let expiry_wake = Arc::new(Notify::new());
    let pacer_wake = Arc::new(Notify::new());

    let panes_task = Arc::clone(&panes);
    let generation_task = Arc::clone(&generation);
    let expiry_task = Arc::clone(&expiry_wake);
    let pacer_task = Arc::clone(&pacer_wake);
    tokio::spawn(run_drain(in_rx, sink, move |batch| {
        let outcome = apply_batch(&batch, &panes_task, &prompt_marks);
        for _ in 0..outcome.redraws {
            generation_task.fetch_add(1, Ordering::Release);
        }
        if outcome.sync_armed {
            expiry_task.notify_one();
        }
        if outcome.frames_queued {
            pacer_task.notify_one();
        }
    }));

    tokio::spawn(run_sync_expiry(Arc::clone(&panes), Arc::clone(&generation), expiry_wake));
    tokio::spawn(run_frame_pacer(panes, generation, pacer_wake));
}

/// Applies one coalesced batch pane by pane, reporting what it owes the redraw
/// generation, the expiry task, and the pacer.
fn apply_batch(
    batch: &CoalescedBatch,
    panes: &Arc<Mutex<PaneGrids>>,
    prompt_marks: &Arc<Mutex<PromptMarks>>,
) -> BatchOutcome {
    let mut outcome = BatchOutcome { redraws: 0, sync_armed: false, frames_queued: false };
    if batch.is_empty() {
        return outcome;
    }
    let batch_panes = resolve_batch_panes(panes, batch);
    for (session, op) in batch.iter() {
        // Each pane advances its own grid, so a background pane's burst can
        // never land in the focused pane's scrollback.
        let Some(pane) = batch_panes.get(&session) else { continue };
        let applied = pane.with_stream(|stream| {
            let redraws = apply_pane_op(op, session, stream, prompt_marks);
            (redraws, stream.sync_armed(), stream.queue.has_frames())
        });
        let Some((redraws, sync_armed, frames_queued)) = applied else { continue };
        outcome.redraws += redraws;
        outcome.sync_armed |= sync_armed;
        outcome.frames_queued |= frames_queued;
    }
    outcome
}

/// Presents the committed bursts pacing left queued, one per pane per redraw
/// interval, so output that arrived faster than the screen can show it still
/// reaches the grid instead of waiting on the next inbound batch.
///
/// Parks whenever every pane is caught up; the drain wakes it as soon as a batch
/// leaves a burst behind. What it walks is bounded without a bound of its own: a
/// pane past [`sync_frames::OUTPUT_FRAME_CATCH_UP_THRESHOLD`] is drained through
/// in a single pass, so a firehose is presented at the batch's own rate and only
/// a caught-up pane is actually paced.
async fn run_frame_pacer(
    panes: Arc<Mutex<PaneGrids>>,
    generation: Arc<AtomicU64>,
    wake: Arc<Notify>,
) {
    loop {
        if !any_pane_has_frames(&panes) {
            wake.notified().await;
            continue;
        }
        tokio::time::sleep(REDRAW_INTERVAL).await;
        let redraws: usize = live_panes(&panes).iter().map(|pane| pane.present_next_burst()).sum();
        for _ in 0..redraws {
            generation.fetch_add(1, Ordering::Release);
        }
    }
}

/// Whether any live pane still owes the pacer a burst.
fn any_pane_has_frames(panes: &Arc<Mutex<PaneGrids>>) -> bool {
    live_panes(panes).iter().any(|pane| pane.has_queued_frames())
}

/// Resolve every pane a batch names into a handle, under one short registry
/// lock, so the parse below runs with that lock released.
fn resolve_batch_panes(
    panes: &Arc<Mutex<PaneGrids>>,
    batch: &CoalescedBatch,
) -> HashMap<SessionId, Arc<PaneGrid>> {
    let Ok(mut grids) = panes.lock() else {
        tracing::warn!("pane registry mutex poisoned; dropping a drained batch");
        return HashMap::new();
    };
    batch.iter().map(|(session, _)| (session, grids.pane(session))).collect()
}

/// Apply one drained operation to a pane, reporting how many repaints it owes.
///
/// The arms share the stream because they are ordered against each other:
/// output advances the grid, a prompt mark reads the row the output left the
/// cursor on, and a legacy `ScrollBottom` can only act at the live tail. A
/// rebuild is the one arm that reshapes the grid before it writes and normalizes
/// legacy replay history afterwards, because it is state rather than a delta —
/// see [`reshape_for_rebuild`].
fn apply_pane_op(
    op: &PaneOp,
    session: SessionId,
    stream: &mut PaneStream,
    prompt_marks: &Arc<Mutex<PromptMarks>>,
) -> usize {
    let PaneStream { queue, terminal: grid } = stream;
    match op {
        PaneOp::Output(bytes) => {
            queue.queue_output_frames(bytes);
            usize::from(present_next_burst(queue, grid))
        }
        PaneOp::Rebuild { bytes, cols, rows, scrollback_rows } => {
            reshape_for_rebuild(session, grid, *cols, *rows);
            let rebuilt = present_rebuild(queue, grid, bytes);
            let kept_rows = usize::try_from(*scrollback_rows).unwrap_or(usize::MAX);
            let trimmed = grid.trim_history(kept_rows);
            usize::from(rebuilt || trimmed > 0)
        }
        PaneOp::PromptMark { kind, exit_code } => {
            apply_prompt_mark(prompt_marks, session, *kind, *exit_code, grid);
            0
        }
        PaneOp::ScrollBottom => {
            // Older servers emitted this after a suppressed ED 3. Never let a
            // delayed legacy frame override a viewport the user is reading.
            if grid.display_offset() != 0 {
                return 0;
            }
            grid.set_split_scroll_eligibility(SplitScrollEligibility::default());
            let moved = grid.scroll(Scroll::Bottom);
            tracing::info!(%session, moved, "processed legacy server ScrollBottom at the live bottom");
            usize::from(moved)
        }
        PaneOp::TrimScrollback { kept_rows } => apply_trim_scrollback(
            prompt_marks,
            session,
            grid.trim_history(*kept_rows),
            *kept_rows,
            grid,
        ),
        PaneOp::TerminalImageLive(message) => match grid.apply_image_live(message.clone()) {
            Ok(committed) => usize::from(committed),
            Err(error) => {
                tracing::warn!(%session, %error, "rejected terminal image live burst");
                0
            }
        },
        PaneOp::TerminalImageReplay(message) => match grid.apply_image_replay(message.clone()) {
            Ok(committed) => usize::from(committed),
            Err(error) => {
                // The staged snapshot is already discarded; the pane keeps the
                // scene it last committed until the server sends a fresh one.
                tracing::warn!(%session, %error, "rejected terminal image replay burst");
                0
            }
        },
    }
}

/// Reshape a pane's grid to the geometry a rebuild was rendered at, before the
/// rebuild bytes reach the parser.
///
/// A rebuild is state, not a delta: the snapshot ANSI emits every row as exactly
/// `cols` printable characters and ends on an absolute CUP in snapshot
/// coordinates, so it only describes the server's screen when the receiving grid
/// is that same shape. The two sides routinely disagree while a window is being
/// dragged — the client reshapes its own grid the instant the layout moves
/// ([`TerminalView::publish_pane_sizes`]) and asks for the authoritative screen in the
/// same breath, while the server debounces its `Term` resize and answers from
/// the size it still has. Replaying a one-column-too-wide rebuild into the
/// narrower grid autowraps every row, scrolls the whole screen into scrollback,
/// and leaves the viewport blank with the cursor parked mid-screen.
///
/// The reshape is therefore driven by the rebuild rather than by the layout: it
/// only has to hold until the size the client actually asked for comes back as
/// a rebuild of its own, which the pending `RequestSnapshot` guarantees.
fn reshape_for_rebuild(session: SessionId, grid: &mut DisplayOnlyTerminal, cols: u16, rows: u16) {
    let (columns, lines) = (usize::from(cols), usize::from(rows));
    let current = grid.dimensions();
    if columns == 0 || lines == 0 || current == (columns, lines) {
        return;
    }
    tracing::info!(
        %session,
        from_cols = current.0,
        from_rows = current.1,
        to_cols = columns,
        to_rows = lines,
        "reshaped a pane to its rebuild's geometry"
    );
    grid.resize(columns, lines);
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
    panes: Arc<Mutex<PaneGrids>>,
    generation: Arc<AtomicU64>,
    wake: Arc<Notify>,
) {
    loop {
        match next_sync_deadline(&panes) {
            None => wake.notified().await,
            Some(deadline) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                tokio::select! {
                    () = tokio::time::sleep(remaining) => flush_expired_sync(&panes, &generation),
                    () = wake.notified() => {}
                }
            }
        }
    }
}

/// Commits every raw-frame and parser synchronized update whose deadline has
/// passed, bumping the redraw generation once per committed burst.
fn flush_expired_sync(panes: &Arc<Mutex<PaneGrids>>, generation: &Arc<AtomicU64>) {
    let now = Instant::now();
    let redraws: usize = live_panes(panes).iter().map(|pane| pane.flush_expired_sync(now)).sum();
    for _ in 0..redraws {
        generation.fetch_add(1, Ordering::Release);
    }
}

/// Nearest synchronized-update deadline across every pane's raw-frame queue and
/// every pane's parser, or `None` when nothing is buffering.
fn next_sync_deadline(panes: &Arc<Mutex<PaneGrids>>) -> Option<Instant> {
    live_panes(panes).iter().filter_map(|pane| pane.sync_deadline()).min()
}

/// Every live pane, resolved under a short registry lock so the expiry task
/// never holds the registry while it waits on a pane the drain is parsing into.
fn live_panes(panes: &Arc<Mutex<PaneGrids>>) -> Vec<Arc<PaneGrid>> {
    panes.lock().map(|grids| grids.panes()).unwrap_or_default()
}

/// Handles owned by the inbound read loop.
struct ReaderCtx {
    /// Whether this connection may act on `Welcome`'s `other_windows` — true
    /// only for the process bootstrap's first, window-less claim.
    fan_out_other_windows: bool,
    /// Per-session display grids, so an exited session's grid can be dropped.
    panes: Arc<Mutex<PaneGrids>>,
    /// Sessions the client has attached; the pane-output gate reads it.
    attached: Arc<Mutex<HashSet<SessionId>>>,
    status: Arc<Mutex<String>>,
    generation: Arc<AtomicU64>,
    active_session: Arc<Mutex<Option<SessionId>>>,
    /// The focused pane's live grid; see the `focused_size` field of `Shared`.
    focused_size: Arc<Mutex<TerminalSize>>,
    /// AI state + prompt history the chrome renders from.
    ai: Arc<Mutex<AiChrome>>,
    /// Server-reported terminal chrome the status bar renders from.
    chrome_metadata: Arc<Mutex<ChromeMetadata>>,
    /// Ordered tab strip the reader rebuilds from server session traffic.
    tabs: Arc<Mutex<TabSessions>>,
    /// Latched by the first `SessionList`; see the `session_list_seen` field of
    /// `Shared`.
    session_list_seen: Arc<AtomicBool>,
    /// One-shot first-shell state; see the `initial_session` field of `Shared`.
    initial_session: Arc<InitialSessionBootstrap>,
    /// Feature-015 share state the reader folds roster and control notices into.
    share: Arc<Mutex<ShareChrome>>,
    /// Update availability / progress the centred status-bar CTA renders from.
    update: Arc<Mutex<UpdateState>>,
    /// Repository-keyed CI snapshots the workspace-region bands render from.
    ci_runs: Arc<Mutex<CiRunBars>>,
    /// Window-lifecycle state the reader adopts a window id into and folds the
    /// server's close / quit / window-list answers onto.
    lifecycle: Arc<Mutex<WindowLifecycle>>,
    /// Bell queue the reader appends to and the foreground's [`BellController`]
    /// drains; see the `bells` field of `Shared`.
    bells: Arc<Mutex<Vec<SessionId>>>,
    /// AI transitions queued for the foreground's notification gate; see the
    /// `ai_notices` field of `Shared`.
    ai_notices: Arc<Mutex<Vec<AiNotice>>>,
    /// Panes whose grid the reattach left unannounced; see the `deferred_grids`
    /// field of `Shared`.
    deferred_grids: Arc<Mutex<Vec<SessionId>>>,
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
    /// The server's persisted workspace tree parked for the shell's reconcile
    /// pass; see the `server_topology` field of `Shared`.
    server_topology: ServerTopologySlot,
    /// Spec-010 OSC 52 state the reader records gating on, parks confirmation
    /// requests in, and queues host clipboard jobs onto.
    clipboard: Arc<Mutex<ClipboardBridge>>,
    /// Feature-013 tailnet state the reader folds every remote answer onto and
    /// queues inbound automation actions in.
    remote: Arc<Mutex<RemoteChrome>>,
    /// Beads-board states written by the reader and painted by the window.
    beads_boards: Arc<Mutex<BeadsBoards>>,
    /// Beads issue detail panels written by the reader and painted by the window.
    beads_panels: Arc<Mutex<BeadsPanels>>,
    out_tx: OutboundSender,
    in_tx: InboundSender,
    sink: IpcSink,
}

/// The grid the reader announces when it attaches a session.
///
/// Reads the focused pane's live size, falling back to the nominal startup box
/// only if the GPUI thread poisoned the lock — which is the same box the window
/// opens at, so a poisoned lock costs at most the resize a first redraw would
/// have published anyway.
fn reader_attach_size(ctx: &ReaderCtx) -> TerminalSize {
    ctx.focused_size.lock().map_or_else(|_| default_terminal_size(), |size| *size)
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
            let at = std::time::SystemTime::now();
            update_ai_chrome(ctx, |ai| ai.apply_state_change(session_id, ai_state, at));
        }
        ServerMessage::AiStateCleared { session_id } => {
            queue_ai_notice(ctx, AiNotice::Cleared { session_id });
            let label_changed =
                ctx.tabs.lock().is_ok_and(|mut tabs| tabs.set_task_label(session_id, None));
            // `forget`, not just the tracker halves: the provider exiting must
            // also take the prompt bar down with it, or the pane keeps a stale
            // prompt history the next conversation never clears.
            update_ai_chrome(ctx, |ai| ai.clear(session_id));
            if label_changed {
                ctx.generation.fetch_add(1, Ordering::Release);
            }
        }
        ServerMessage::PromptReceived { session_id, text, .. } => {
            let at = std::time::SystemTime::now();
            update_ai_chrome(ctx, |ai| ai.record_prompt(session_id, &text, at));
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

/// Ask the server to permanently close this window through the one shared
/// request/acknowledgement gate used by UI close actions and a final PTY exit.
// @lat: [[client#Client#Dialogs#Close Dialog]]
fn request_permanent_window_close(lifecycle: &Mutex<WindowLifecycle>, sink: &IpcSink) {
    let Ok(mut lifecycle_state) = lifecycle.lock() else {
        tracing::warn!("close window dropped: window lifecycle mutex poisoned");
        return;
    };
    let Some(window_id) = lifecycle_state.begin_close_window() else {
        tracing::warn!(
            "close window ignored: no window id from Welcome yet, or a shutdown is in flight"
        );
        return;
    };
    drop(lifecycle_state);
    tracing::info!(%window_id, "closing window permanently — awaiting server acknowledgment");
    if let Err(error) = sink.close_window(window_id) {
        tracing::warn!(%error, "close window dropped: IPC writer closed");
        if let Ok(mut retry_state) = lifecycle.lock() {
            retry_state.abandon_shutdown();
        }
    }
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

/// Fold a LAN warning/error and mirror it onto the status bar.
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

/// Fold a tailnet warning/error and mirror it onto the status bar.
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
    chrome_bg: gpui::Rgba,
    opacity: f32,
) -> gpui::Rgba {
    if placement.focused {
        // The region's darker tab tone, matching the workspace tag and the
        // strip hairline, rather than the raw accent.
        let tone =
            scribe_client::tab_bar::accent_tab_tone(opaque_slot(placement.accent), chrome_bg);
        scribe_client::opacity::scale_alpha(tone, opacity)
    } else {
        idle
    }
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
        ServerMessage::IconTitleChanged { .. } => "IconTitleChanged",
        ServerMessage::CodexTaskLabelChanged { .. } => "CodexTaskLabelChanged",
        ServerMessage::CodexTaskLabelCleared { .. } => "CodexTaskLabelCleared",
        ServerMessage::TaskLabelChanged { .. } => "TaskLabelChanged",
        ServerMessage::TaskLabelCleared { .. } => "TaskLabelCleared",
        ServerMessage::PromptReceived { .. } => "PromptReceived",
        ServerMessage::WorkspaceNamed { .. } => "WorkspaceNamed",
        ServerMessage::CiRunState { .. } => "CiRunState",
        ServerMessage::CiRunDetails { .. } => "CiRunDetails",
        ServerMessage::SessionCreated { .. } => "SessionCreated",
        ServerMessage::SessionExited { .. } => "SessionExited",
        ServerMessage::Bell { .. } => "Bell",
        ServerMessage::Error { .. } => "Error",
        ServerMessage::GitBranch { .. } => "GitBranch",
        ServerMessage::SessionList { .. } => "SessionList",
        ServerMessage::WorkspaceInfo { .. } => "WorkspaceInfo",
        ServerMessage::BeadsBoard { .. } => "BeadsBoard",
        ServerMessage::BeadsIssueDetail { .. } => "BeadsIssueDetail",
        ServerMessage::BeadsIssueWriteResult { .. } => "BeadsIssueWriteResult",
        ServerMessage::SearchResults { .. } => "SearchResults",
        ServerMessage::Welcome { .. } => "Welcome",
        ServerMessage::TerminalImageLive { .. } => "TerminalImageLive",
        ServerMessage::TerminalImageReplay { .. } => "TerminalImageReplay",
        ServerMessage::TerminalImageCapabilityMismatch { .. } => "TerminalImageCapabilityMismatch",
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

fn request_beads_board_or_log(sink: &IpcSink, workspace_id: WorkspaceId, reason: &'static str) {
    if let Err(error) = sink.request_beads_board(workspace_id) {
        tracing::debug!(%error, reason, "Beads board request dropped");
    }
}

fn include_open_panel_boards(
    visible: &mut Vec<(WorkspaceId, bool)>,
    panel_workspaces: &[WorkspaceId],
    boards: &BeadsBoards,
) {
    for workspace_id in panel_workspaces {
        if !visible.iter().any(|(visible_id, _)| visible_id == workspace_id) {
            visible.push((*workspace_id, boards.is_pinned(*workspace_id)));
        }
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
    let (refocused, tabs_empty) = ctx.tabs.lock().map_or((None, false), |mut tabs| {
        let refocused = tabs.remove(session_id);
        (refocused, tabs.is_empty())
    });
    if existed && tabs_empty {
        request_permanent_window_close(&ctx.lifecycle, &ctx.sink);
    } else if let Some(next) = refocused {
        attach_session(ctx, next)?;
    }
    if existed && Some(session_id) == attached {
        set_status(&ctx.status, &ctx.generation, "attached pane exited".to_owned());
    }
    Ok(())
}

/// Repaint the attached pane from a `ScreenSnapshot` the client asked for.
///
/// The snapshot's ANSI starts with RIS, so the pane is replaced rather than
/// appended onto — everything on screen afterwards came out of this snapshot.
/// That is also what makes the repaint assertable from
/// outside the process, which the `RequestSnapshot` E2E relies on, so the
/// applied grid's dimensions are logged alongside the session.
///
/// The snapshot's own `cols`/`rows` and `scrollback_rows` go with the bytes
/// rather than being dropped here: they are the geometry and history count the
/// ANSI must reproduce, so the drain reshapes before replay and normalizes any
/// synthetic history carried by a legacy pre-RIS replay afterwards. The
/// client's own grid has usually already moved on:
/// [`TerminalView::publish_pane_sizes`] reshapes the local grid the moment the
/// window changes size, while the server still answers the `RequestSnapshot`
/// from the size its `Term` had before its own resize landed.
fn apply_screen_snapshot(ctx: &ReaderCtx, session_id: SessionId, snapshot: &ScreenSnapshot) {
    let bytes = scribe_common::screen_replay::snapshot_to_ansi(snapshot);
    forward_inbound(
        &ctx.in_tx,
        InboundEvent::PaneRebuild {
            session_id,
            bytes,
            cols: snapshot.cols,
            rows: snapshot.rows,
            scrollback_rows: snapshot.scrollback_rows,
        },
    );
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
///
/// The inflate itself runs on the blocking pool: this thread's runtime also
/// owns the writer and the drain, so a large replay decoded inline would hold
/// back keystrokes and pane repaints for as long as zstd ran.
async fn forward_replay(ctx: &ReaderCtx, session_id: SessionId, replay: SessionReplay) {
    let (cols, rows, scrollback_rows) = (replay.cols, replay.rows, replay.scrollback_rows);
    match session_lifecycle::decode_replay_off_thread(session_id, replay).await {
        Ok(bytes) => {
            forward_inbound(
                &ctx.in_tx,
                InboundEvent::PaneRebuild { session_id, bytes, cols, rows, scrollback_rows },
            );
        }
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
        dispatch_server_message(message, &ctx, &mut registry, attached, first_session_list).await?;
        if is_session_list {
            first_session_list = false;
        }
        // This window's own close was just acknowledged. The server does not
        // hang up — it answered and moved on — so nothing else ever ends this
        // reader, and the window it serves is already being torn down on the
        // GPUI thread. Stop here rather than draining frames for sessions the
        // server destroyed.
        if ctx.lifecycle.lock().is_ok_and(|lifecycle| lifecycle.window_closed()) {
            return Ok(());
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
#[allow(
    clippy::too_many_lines,
    reason = "exhaustive protocol routing stays auditable in one table"
)]
async fn dispatch_server_message(
    message: ServerMessage,
    ctx: &ReaderCtx,
    registry: &mut session_lifecycle::SessionRegistry,
    attached: Option<SessionId>,
    first_session_list: bool,
) -> Result<(), String> {
    match message {
        welcome @ ServerMessage::Welcome { .. } => on_welcome(ctx, registry, welcome),
        ServerMessage::SessionList { sessions, workspaces, workspace_tree } => {
            park_server_topology(ctx, &sessions, workspace_tree, first_session_list);
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
        // OSC 133 marks, legacy ScrollBottom, and AI scrollback trims are all
        // positional: each describes the grid after output the server already
        // sent, so they share its ordered inbound channel rather than applying
        // here.
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
        | ServerMessage::ScreenSnapshot { .. }) => on_pane_output_message(ctx, output).await,
        image @ (ServerMessage::TerminalImageLive { .. }
        | ServerMessage::TerminalImageReplay { .. }
        | ServerMessage::TerminalImageCapabilityMismatch { .. }) => {
            on_terminal_image_message(ctx, image);
        }
        // The terminal-chrome family all lands in the same two stores (the tab
        // strip's labels and the shared metadata), so it is named here and
        // routed as one to [`on_chrome_message`].
        chrome @ (ServerMessage::TitleChanged { .. }
        | ServerMessage::IconTitleChanged { .. }
        | ServerMessage::CwdChanged { .. }
        | ServerMessage::GitBranch { .. }
        | ServerMessage::SessionContextChanged { .. }
        | ServerMessage::EnvStatus { .. }
        | ServerMessage::WorkspaceNamed { .. }) => on_chrome_message(ctx, chrome),
        info @ ServerMessage::WorkspaceInfo { .. } => on_workspace_info(ctx, info),
        ServerMessage::BeadsBoard { workspace_id, state, .. } => {
            let loading = matches!(state, scribe_common::protocol::BeadsBoardState::Loading { .. });
            if let Ok(mut panels) = ctx.beads_panels.lock() {
                panels.sync_board(workspace_id, &state);
            }
            let classifier_won = ctx.beads_boards.lock().map_or_else(
                |_| Vec::new(),
                |mut boards| {
                    let classifier_won = boards.update(workspace_id, state);
                    ctx.generation.fetch_add(1, Ordering::Release);
                    classifier_won
                },
            );
            if let Some((issue_id, lane)) = classifier_won.into_iter().last()
                && let Ok(mut panels) = ctx.beads_panels.lock()
            {
                panels.classifier_won(workspace_id, &issue_id, lane);
            }
            if loading {
                let sink = ctx.sink.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    request_beads_board_or_log(&sink, workspace_id, "loading retry");
                });
            }
        }
        ServerMessage::BeadsIssueDetail { workspace_id, issue_id, detail } => {
            if let Ok(mut panels) = ctx.beads_panels.lock() {
                panels.update(workspace_id, &issue_id, detail);
                ctx.generation.fetch_add(1, Ordering::Release);
            }
        }
        ServerMessage::BeadsIssueWriteResult { workspace_id, issue_id, result } => {
            if let Ok(mut boards) = ctx.beads_boards.lock() {
                boards.finish_card_drop(workspace_id, &issue_id, &result);
            }
            if let Ok(mut panels) = ctx.beads_panels.lock() {
                panels.finish_write(workspace_id, &issue_id, result);
                ctx.generation.fetch_add(1, Ordering::Release);
            }
        }
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
        ci @ (ServerMessage::CiRunState { .. } | ServerMessage::CiRunDetails { .. }) => {
            on_ci_run_message(ctx, ci);
        }
        ServerMessage::Bell { session_id } => on_bell_message(ctx, session_id),
        ServerMessage::SearchResults { session_id, query, matches } => {
            on_search_results(ctx, session_id, query, matches, attached);
        }
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

/// Park the server's persisted split tree for the GPUI thread's reconcile
/// pass.
///
/// Runs *before* [`on_session_list`] rebuilds the tab strip, so no reconcile
/// pass can see the sessions without the tree and lay them out flat first.
/// Only the first list of a connection carries a layout worth adopting — a
/// later refresh describes sessions the window already shows — and the GPUI
/// thread additionally ignores the tree when the window already has a layout
/// of its own.
fn park_server_topology(
    ctx: &ReaderCtx,
    sessions: &[SessionInfo],
    workspace_tree: Option<WorkspaceTreeNode>,
    first_on_connection: bool,
) {
    if !first_on_connection || sessions.is_empty() {
        return;
    }
    let Some(tree) = workspace_tree else { return };
    let live: HashSet<SessionId> = sessions.iter().map(|session| session.session_id).collect();
    if let Ok(mut parked) = ctx.server_topology.lock() {
        *parked = Some((tree, live));
    } else {
        tracing::warn!("server topology mutex poisoned; dropping workspace tree");
    }
}

/// Adopt the server's session inventory: rebuild the reconnect topology, park
/// workspace metadata for the pane shell, seed chrome, and reconcile the tabs.
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
    // The list is authoritative for the same workspace fields as a standalone
    // `WorkspaceInfo`. Park its entries on the reader-owned queue so a fresh
    // client restores project roots instead of waiting for a later CWD change.
    // Message dispatch is ordered, so a following live `WorkspaceNamed` update
    // is appended after this snapshot and wins when the foreground drains it.
    park_workspace_info(
        ctx,
        workspaces.iter().map(|workspace| {
            let accent = scribe_common::theme::hex_to_rgba(&workspace.accent_color).ok();
            if accent.is_none() {
                tracing::warn!(
                    workspace_id = %workspace.workspace_id,
                    accent_color = workspace.accent_color,
                    "workspace accent is not a #rrggbb colour"
                );
            }
            WorkspaceInfo {
                workspace_id: workspace.workspace_id,
                name: workspace.name.clone(),
                accent,
                project_root: workspace.project_root.clone(),
            }
        }),
    );
    update_chrome_metadata(ctx, |metadata| {
        metadata.seed_from_session_list(sessions, workspaces);
    });
    if first_on_connection {
        seed_ai_chrome_from_session_list(ctx, sessions);
        reattach_visible_sessions(ctx, sessions)?;
    }
    let attached = ctx.active_session.lock().ok().and_then(|guard| *guard);
    sync_tab_strip(ctx, sessions, attached)?;
    request_initial_session(ctx, sessions.len(), first_on_connection)
}

/// Seed [`AiChrome`] from the server's view of every listed session.
///
/// The counterpart to [`ChromeMetadata::seed_from_session_list`] for the AI
/// half of the chrome. A client that reconnects to a surviving server gets its
/// prompt bars and AI indicators back from this list instead of staying blank
/// until the provider happens to emit the next hook event — which, for an idle
/// conversation, may be never.
///
/// Only the first list of a connection seeds: a later list is a topology
/// refresh, and re-applying `ai_state` there would resurrect an attention
/// state the user has already dismissed with a keystroke
/// ([`AiStateTracker::clear_attention_states`]), which the server never hears
/// about.
fn seed_ai_chrome_from_session_list(ctx: &ReaderCtx, sessions: &[SessionInfo]) {
    update_ai_chrome(ctx, |ai| ai.seed_from_session_list(sessions));
}

/// Create the login shell that makes a genuinely fresh window useful.
///
/// Cold-restart windows enter with the one-shot disarmed so their persisted
/// launches remain the sole source of sessions. Existing-window claims are
/// disarmed before `Hello`; a non-empty first list consumes the one-shot too,
/// preserving server-owned sessions without adding another tab.
fn request_initial_session(
    ctx: &ReaderCtx,
    session_count: usize,
    first_on_connection: bool,
) -> Result<(), String> {
    if !ctx.initial_session.claim(first_on_connection, session_count) {
        return Ok(());
    }

    let workspace_id = WorkspaceId::new();
    let binding = restore_replay::new_shell_binding(None);
    let launch_id = binding.launch_id.clone();
    let Ok(mut pending) = ctx.initial_session.binding.lock() else {
        ctx.initial_session.armed.store(true, Ordering::Release);
        return Err("initial-session binding mutex poisoned".to_owned());
    };
    *pending = Some(binding);
    drop(pending);

    if let Err(error) = ctx.sink.create_session(SessionLaunch {
        workspace_id,
        size: reader_attach_size(ctx),
        cwd: None,
        command: None,
        ai_launch: None,
        launch_id,
    }) {
        // The request never entered the ordered queue, so let the next
        // connection retry rather than leaving this fresh window empty.
        ctx.initial_session.armed.store(true, Ordering::Release);
        return Err(error.to_string());
    }
    tracing::info!(%workspace_id, "requested initial shell session");
    Ok(())
}

/// Whether the server's view of a session says a Codex process owns it.
///
/// The live state's own provider wins; `ai_provider_hint` is the fallback for a
/// session whose visible state has already been cleared but whose
/// provider-aware behaviour must survive the reattach.
fn is_codex_session(info: &SessionInfo) -> bool {
    info.ai_state.as_ref().map(|state| state.provider).or(info.ai_provider_hint)
        == Some(scribe_common::ai_state::AiProvider::CodexCode)
}

/// One retained pane's reattach: the grid its `AttachSessions` announces, and
/// whether the reconnect may follow that attach with a `Resize`.
struct ReattachPane {
    session_id: SessionId,
    size: TerminalSize,
    resize_now: bool,
}

/// Decide what each retained pane announces when its connection is replaced.
///
/// A Codex pane is the exception the retired client encoded and this one lost:
/// it announces a zero-sized grid and takes no follow-up `Resize`. The server's
/// `attach_flow::send_attach_replay` runs its pre-replay `resize_term` +
/// `TIOCSWINSZ` only for a size that has a grid, and Codex renders through Ink,
/// which repaints on `SIGWINCH` and would paint over the history the replay has
/// just restored. The real grid is not lost — it is deferred to the ordinary
/// publish cycle, which is why the caller also parks the pane in
/// `Shared::deferred_grids`.
fn reattach_panes(
    sessions: &[SessionInfo],
    retained: &[SessionId],
    grids: &[GridSize],
    cell_size: (f32, f32),
) -> Vec<ReattachPane> {
    let codex: HashSet<SessionId> =
        sessions.iter().filter(|info| is_codex_session(info)).map(|info| info.session_id).collect();
    retained
        .iter()
        .zip(grids)
        .map(|(session_id, grid)| {
            let is_codex = codex.contains(session_id);
            ReattachPane {
                session_id: *session_id,
                size: attach_dimensions_for_session(Some(*grid), cell_size, is_codex),
                resize_now: !is_codex,
            }
        })
        .collect()
}

/// Reattach panes retained by the live window to a replacement server stream.
///
/// The local `attached` set describes which sessions the window still shows,
/// but an `AttachSessions` grant belongs to one IPC connection. A server
/// handoff therefore needs to replay the set after the replacement connection's
/// first `SessionList`. Dimensions come from each pane's existing display grid
/// so split panes keep their geometry across the handoff — except for a Codex
/// pane, which [`reattach_panes`] defers to the next publish.
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

    let focused = reader_attach_size(ctx);
    let fallback = GridSize { cols: focused.cols, rows: focused.rows };
    let grids: Vec<GridSize> = ctx.panes.lock().map_or_else(
        |_| vec![fallback; retained.len()],
        |panes| {
            retained
                .iter()
                .map(|session_id| pane_display_grid(&panes, *session_id).unwrap_or(fallback))
                .collect()
        },
    );
    let cell_size = (f32::from(focused.cell_width), f32::from(focused.cell_height));
    let panes = reattach_panes(sessions, &retained, &grids, cell_size);

    tracing::info!(sessions = retained.len(), "reattaching visible sessions");
    for pane in &panes {
        tracing::info!(
            session_id = %pane.session_id,
            cols = pane.size.cols,
            rows = pane.size.rows,
            resize_now = pane.resize_now,
            "attaching to session"
        );
    }
    ctx.out_tx
        .send(ClientMessage::AttachSessions {
            session_ids: retained.clone(),
            dimensions: panes.iter().map(|pane| pane.size).collect(),
        })
        .map_err(|error| error.to_string())?;
    for pane in panes.iter().filter(|pane| pane.resize_now) {
        ctx.sink.resize(pane.session_id, pane.size).map_err(|error| error.to_string())?;
    }
    // The GPUI half of the retired client's `mark_reconnected_grids`: a pane
    // that announced no grid must not stay at the size the view last published,
    // or `publish_pane_sizes` would skip it forever and the server would never
    // hear the real one.
    if let Ok(mut deferred) = ctx.deferred_grids.lock() {
        deferred.extend(panes.iter().filter(|pane| !pane.resize_now).map(|pane| pane.session_id));
    } else {
        tracing::warn!("deferred-grid queue poisoned; a reattached pane may keep a stale size");
    }
    ctx.sink.subscribe(retained).map_err(|error| error.to_string())
}

/// A pane's current display grid, when it has one the protocol can carry.
fn pane_display_grid(panes: &PaneGrids, session_id: SessionId) -> Option<GridSize> {
    let (cols, rows) = panes.dimensions(session_id)?;
    Some(GridSize { cols: u16::try_from(cols).ok()?, rows: u16::try_from(rows).ok()? })
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

/// Surface one server rejection on the status line.
fn on_server_error(ctx: &ReaderCtx, message: String) {
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
    tracing::info!(%workspace_id, ?name, accent_color, "workspace info received");
    park_workspace_info(ctx, [WorkspaceInfo { workspace_id, name, accent, project_root }]);
    ctx.generation.fetch_add(1, Ordering::Release);
}

/// Park server-owned workspace metadata for the GPUI thread, asking for the
/// Beads board of every entry.
///
/// Parking and asking are one step on purpose. A workspace gaining a root must
/// discover its board, while one losing its root must receive `NotDetected` so
/// an old pinned board closes. The session list, `WorkspaceInfo`, and
/// `WorkspaceNamed` all route through here so neither transition gets parked
/// without a request.
fn park_workspace_info(ctx: &ReaderCtx, infos: impl IntoIterator<Item = WorkspaceInfo>) {
    let Ok(mut parked) = ctx.workspaces.lock() else {
        tracing::warn!("workspace info mutex poisoned; dropping workspace metadata");
        return;
    };
    let first_new = parked.len();
    parked.extend(infos);
    let updated: Vec<WorkspaceId> =
        parked.get(first_new..).unwrap_or_default().iter().map(|info| info.workspace_id).collect();
    drop(parked);
    for workspace_id in updated {
        request_beads_board_or_log(&ctx.sink, workspace_id, "workspace metadata parked");
    }
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
            if ctx.tabs.lock().is_ok_and(|mut tabs| tabs.set_title(session_id, Some(title))) {
                ctx.generation.fetch_add(1, Ordering::Release);
            }
        }
        ServerMessage::IconTitleChanged { session_id, title } => {
            if ctx.tabs.lock().is_ok_and(|mut tabs| tabs.set_icon_title(session_id, Some(title))) {
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
        ServerMessage::WorkspaceNamed { workspace_id, name, project_root } => {
            // Auto-naming is also the live project-root update. Park it for
            // the GPUI thread before bumping the redraw generation so the
            // focused workspace slot and status-bar metadata advance together.
            // Auto-naming is where most workspaces first gain a root: a fresh
            // server cannot name one until a shell reports a CWD, which is
            // after the session list that seeds the eager requests.
            park_workspace_info(
                ctx,
                [WorkspaceInfo {
                    workspace_id,
                    name: (!name.is_empty()).then(|| name.clone()),
                    accent: None,
                    project_root,
                }],
            );
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
async fn on_pane_output_message(ctx: &ReaderCtx, message: ServerMessage) {
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
                forward_replay(ctx, session_id, replay).await;
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

/// Route live image records through the pane FIFO and show typed attach
/// mismatches in the existing window status bar.
fn on_terminal_image_message(ctx: &ReaderCtx, message: ServerMessage) {
    match message {
        ServerMessage::TerminalImageLive { session_id, message } => {
            if is_attached(ctx, session_id) {
                forward_inbound(
                    &ctx.in_tx,
                    InboundEvent::TerminalImageLive { session_id, message },
                );
            }
        }
        ServerMessage::TerminalImageReplay { session_id, message } => {
            if is_attached(ctx, session_id) {
                forward_inbound(
                    &ctx.in_tx,
                    InboundEvent::TerminalImageReplay { session_id, message },
                );
            }
        }
        ServerMessage::TerminalImageCapabilityMismatch { session_id, mismatch } => {
            tracing::warn!(%session_id, ?mismatch, "terminal image capability mismatch");
            set_status(&ctx.status, &ctx.generation, capability_mismatch_message(mismatch));
        }
        other => unhandled_server_message(&other),
    }
}

/// Forward one positional pane event onto the ordered inbound channel.
///
/// A prompt mark's anchor row and a legacy `ScrollBottom` only mean anything
/// relative to the output around them, so both travel the same FIFO the pane's
/// bytes do and are applied by the drain, not here. Anything other than those
/// variants is a routing error in [`dispatch_server_message`], not a protocol
/// event, so it is counted as unhandled rather than silently dropped.
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

/// Fold one repository-scoped CI replacement or clear onto workspace chrome.
fn on_ci_run_message(ctx: &ReaderCtx, message: ServerMessage) {
    let Ok(mut runs) = ctx.ci_runs.lock() else {
        tracing::warn!("CI run state mutex poisoned; dropping update");
        return;
    };
    match message {
        ServerMessage::CiRunState { repo_root, delta } => runs.apply(repo_root, delta),
        ServerMessage::CiRunDetails { repo_root, details } => {
            runs.apply_details(repo_root, details);
        }
        other => {
            drop(runs);
            unhandled_server_message(&other);
            return;
        }
    }
    drop(runs);
    ctx.generation.fetch_add(1, Ordering::Release);
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
        }
        ServerMessage::LanPeerList { peers } => {
            tracing::info!(count = peers.len(), "server LAN peer list");
            update_lan_chrome(ctx, |lan| lan.set_peers(peers));
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
            let update = |lan: &mut LanChrome| {
                lan.set_env(LanEnvSummary {
                    device_id_hex,
                    fingerprint_words,
                    current_network_addable,
                    current_network_reason,
                });
            };
            if current_network_addable {
                update_lan_chrome(ctx, update);
                set_status(&ctx.status, &ctx.generation, String::new());
            } else {
                update_lan_chrome_and_status(ctx, update);
            }
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
            if outcome == LanConnectOutcome::Accepted {
                update_lan_chrome(ctx, |lan| lan.settle_dial(outcome));
                set_status(&ctx.status, &ctx.generation, String::new());
            } else {
                update_lan_chrome_and_status(ctx, |lan| lan.settle_dial(outcome));
            }
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
            let update = |remote: &mut RemoteChrome| {
                remote.set_env(RemoteEnvSummary { account, tailscale_detected });
            };
            if tailscale_detected {
                update_remote_chrome(ctx, update);
                set_status(&ctx.status, &ctx.generation, String::new());
            } else {
                update_remote_chrome_and_status(ctx, update);
            }
        }
        ServerMessage::RemotePeerList { peers } => {
            tracing::info!(count = peers.len(), "server tailnet peer list");
            update_remote_chrome(ctx, |remote| remote.set_peers(peers));
        }
        ServerMessage::RemoteHandshakeReply {
            accepted,
            refusal,
            server_remote_protocol_version,
            server_scribe_version,
            version_mismatch: _,
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
            if outcome == RemoteConnectOutcome::Accepted {
                update_remote_chrome(ctx, |remote| remote.settle_dial(outcome));
                set_status(&ctx.status, &ctx.generation, String::new());
            } else {
                update_remote_chrome_and_status(ctx, |remote| remote.settle_dial(outcome));
            }
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
            update_remote_chrome(ctx, |remote| {
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
///
/// `other_windows` is the rest of the user's windows: the server kept their
/// sessions when the client exited and handed this connection only one of them,
/// so the remainder have to be reopened or they simply do not come back. They
/// are parked for the foreground, which owns window creation.
fn on_welcome(
    ctx: &ReaderCtx,
    registry: &mut session_lifecycle::SessionRegistry,
    welcome: ServerMessage,
) {
    let ServerMessage::Welcome {
        window_id,
        other_windows,
        participant_id,
        clipboard_gating,
        beads_detail,
        beads_write,
        ..
    } = welcome
    else {
        return;
    };
    registry.adopt_window(window_id);
    update_lifecycle(ctx, |lifecycle| lifecycle.adopt_window(window_id));
    if ctx.fan_out_other_windows && !other_windows.is_empty() {
        tracing::info!(
            count = other_windows.len(),
            "welcome: reopening the server's other windows"
        );
        update_lifecycle(ctx, |lifecycle| lifecycle.park_sibling_windows(other_windows));
    }
    update_share_chrome(ctx, |share| share.set_self_id(participant_id));
    // Spec 010 C7: the server echoes back whether it will route OSC 52 through
    // this client. Recording it here is what lets the clipboard arms below
    // refuse to act on a frame that arrived without a negotiated capability.
    if let Ok(mut bridge) = ctx.clipboard.lock() {
        bridge.set_gating(clipboard_gating);
    }
    if let Ok(mut panels) = ctx.beads_panels.lock() {
        panels.set_enabled(beads_detail);
        panels.set_write_enabled(beads_write);
        panels.reconnected();
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

/// Add a freshly created session to the tab strip, focus it, and make it stream.
///
/// The server also re-announces `SessionCreated` to acknowledge every
/// `AttachSessions`, so only a genuine insert counts as a new tab — attaching
/// on the echo would attach in an unbounded loop.
///
/// Which of the two routes a genuine insert takes is decided by
/// [`IpcSink::claim_pending_create`]: the answer to a `CreateSession` this
/// window sent is already attached and already at the geometry the request
/// named, so it is adopted rather than re-attached.
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
        if ctx.sink.claim_pending_create() {
            adopt_created_session(ctx, session_id)?;
        } else {
            attach_session(ctx, session_id)?;
        }
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
    let focused = ctx.tabs.lock().ok().and_then(|mut tabs| tabs.reconcile(entries, attached));
    match focused {
        Some(session_id) if Some(session_id) != attached => {
            attach_session(ctx, session_id)?;
            ctx.generation.fetch_add(1, Ordering::Release);
        }
        // Already attached to the focused tab (or nothing focused); just
        // repaint so any label change carried by the list lands in the tab row.
        Some(_) | None => {
            ctx.generation.fetch_add(1, Ordering::Release);
        }
    }
    Ok(())
}

/// Lower a server [`SessionInfo`] into a tab strip entry.
///
/// Native OSC 0/2 title owns the label. A provider task label is retained as a
/// fallback while no native title exists, then the shell basename wins.
fn tab_entry_for(info: &SessionInfo) -> TabEntry {
    let mut entry = TabEntry::new(info.session_id, info.workspace_id, info.shell_name.clone());
    entry.terminal_title = info.title.clone().filter(|title| !title.trim().is_empty());
    entry.icon_title = info.icon_title.clone().filter(|title| !title.trim().is_empty());
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
    let size = reader_attach_size(ctx);
    tracing::info!(%session_id, cols = size.cols, rows = size.rows, "attaching to session");
    ctx.out_tx
        .send(ClientMessage::AttachSessions {
            session_ids: vec![session_id],
            dimensions: vec![size],
        })
        .map_err(|error| error.to_string())?;
    // Announce the client size through the sink, ahead of any KeyInput.
    ctx.sink.resize(session_id, size).map_err(|error| error.to_string())?;
    ctx.sink.subscribe(vec![session_id]).map_err(|error| error.to_string())?;
    adopt_attached_session(ctx, session_id);
    Ok(())
}

/// Take up a session the server attached on this client's behalf.
///
/// A `CreateSession` is an attach: the server installs this connection's sink
/// while it starts the session and records the id in this connection's attached
/// set, so the answer already arrives with the pane streaming. What is left is
/// the client's own half of the same bookkeeping plus the `Subscribe` every
/// visible pane needs — see [`attach_session`] for why that frame has to follow
/// the attach on the ordered channel, which a create satisfies by having *been*
/// the attach.
///
/// Deliberately sends neither `AttachSessions` nor `Resize`. The attach would
/// re-point a sink that is already this connection's and pay for a full
/// redundant full-state replay that can overwrite shell startup bytes already
/// delivered to the pane; the resize would drive the PTY off the geometry the
/// create just spawned it at and back again.
fn adopt_created_session(ctx: &ReaderCtx, session_id: SessionId) -> Result<(), String> {
    tracing::info!(%session_id, "adopting a freshly created session; already attached");
    ctx.sink.subscribe(vec![session_id]).map_err(|error| error.to_string())?;
    adopt_attached_session(ctx, session_id);
    Ok(())
}

/// Record that this client now streams and types into `session_id`.
fn adopt_attached_session(ctx: &ReaderCtx, session_id: SessionId) {
    if let Ok(mut attached) = ctx.attached.lock() {
        attached.insert(session_id);
    }
    if let Ok(mut guard) = ctx.active_session.lock() {
        *guard = Some(session_id);
    }
}

fn forward_output(in_tx: &InboundSender, session_id: SessionId, bytes: Vec<u8>) {
    forward_inbound(in_tx, InboundEvent::PaneOutput { session_id, bytes });
}

/// Hand one event to the coalescing drain, preserving arrival order.
///
/// Every pane-affecting message goes through here so output and positional
/// events interleaved with it (prompt marks and legacy `ScrollBottom`) cannot be
/// reordered relative to each other. The queue behind it is bounded and never
/// blocks the reader: an overflow is absorbed by the drain's resync rather than
/// by stalling the socket read.
fn forward_inbound(in_tx: &InboundSender, event: InboundEvent) {
    if in_tx.send(event).is_err() {
        tracing::warn!("inbound drain closed; dropping pane event");
    }
}

fn set_status(status: &Arc<Mutex<String>>, generation: &AtomicU64, message: String) {
    // The status string is deliberately not rendered anywhere: transient
    // connection / pane errors are internal plumbing noise, so the log is
    // their only user-reachable surface.
    if !message.is_empty() {
        tracing::warn!(%message, "window status");
    }
    if let Ok(mut status) = status.lock() {
        *status = message;
        generation.fetch_add(1, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use std::ops::Range;

    use gpui::{ElementInputHandler, EntityInputHandler, UTF16Selection};
    use scribe_common::screen::{CellFlags, CursorStyle, ScreenCell, ScreenColor};

    use super::*;

    #[test]
    fn hover_focus_requires_enabled_button_free_different_pane() {
        let focused = SessionId::new();
        let hovered = SessionId::new();

        assert_eq!(hover_focus_target(true, None, Some(hovered), Some(focused)), Some(hovered));
        assert_eq!(hover_focus_target(false, None, Some(hovered), Some(focused)), None);
        assert_eq!(
            hover_focus_target(true, Some(MouseButton::Left), Some(hovered), Some(focused)),
            None
        );
        assert_eq!(hover_focus_target(true, None, None, Some(focused)), None);
        assert_eq!(hover_focus_target(true, None, Some(focused), Some(focused)), None);
    }

    struct EditorInputProbe {
        focus: FocusHandle,
        text: String,
    }

    impl EntityInputHandler for EditorInputProbe {
        fn text_for_range(
            &mut self,
            _range: Range<usize>,
            _actual_range: &mut Option<Range<usize>>,
            _window: &mut Window,
            _cx: &mut Context<Self>,
        ) -> Option<String> {
            Some(self.text.clone())
        }

        fn selected_text_range(
            &mut self,
            _ignore_disabled_input: bool,
            _window: &mut Window,
            _cx: &mut Context<Self>,
        ) -> Option<UTF16Selection> {
            let end = self.text.encode_utf16().count();
            Some(UTF16Selection { range: end..end, reversed: false })
        }

        fn marked_text_range(
            &self,
            _window: &mut Window,
            _cx: &mut Context<Self>,
        ) -> Option<Range<usize>> {
            None
        }

        fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {}

        fn replace_text_in_range(
            &mut self,
            _range: Option<Range<usize>>,
            text: &str,
            _window: &mut Window,
            cx: &mut Context<Self>,
        ) {
            self.text.push_str(text);
            cx.notify();
        }

        fn replace_and_mark_text_in_range(
            &mut self,
            _range: Option<Range<usize>>,
            new_text: &str,
            _new_selected_range: Option<Range<usize>>,
            _window: &mut Window,
            cx: &mut Context<Self>,
        ) {
            self.text.push_str(new_text);
            cx.notify();
        }

        fn bounds_for_range(
            &mut self,
            _range: Range<usize>,
            element_bounds: Bounds<Pixels>,
            _window: &mut Window,
            _cx: &mut Context<Self>,
        ) -> Option<Bounds<Pixels>> {
            Some(element_bounds)
        }

        fn character_index_for_point(
            &mut self,
            _point: Point<Pixels>,
            _window: &mut Window,
            _cx: &mut Context<Self>,
        ) -> Option<usize> {
            Some(self.text.encode_utf16().count())
        }
    }

    struct EditorWindowProbe {
        editor: Entity<EditorInputProbe>,
        terminal_focus: FocusHandle,
        editor_keys: Vec<String>,
        pty_bytes: Vec<Vec<u8>>,
    }

    impl EditorWindowProbe {
        fn route_key(&mut self, event: &KeyDownEvent, window: &Window, cx: &mut Context<Self>) {
            if self.editor.read(cx).focus.is_focused(window) {
                self.editor_keys.push(event.keystroke.key.clone());
                return;
            }
            cx.stop_propagation();
            if let Some(bytes) = encode_key(event) {
                self.pty_bytes.push(bytes);
            }
        }
    }

    impl Render for EditorWindowProbe {
        fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let editor = self.editor.clone();
            let editor_focus = editor.read(cx).focus.clone();
            let input_focus = editor_focus.clone();
            div()
                .track_focus(&self.terminal_focus)
                .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                    this.route_key(event, window, cx);
                }))
                .size_full()
                .child(
                    div().track_focus(&editor_focus).size_full().child(
                        canvas(
                            |_, _, _| {},
                            move |bounds, (), window, app| {
                                window.handle_input(
                                    &input_focus,
                                    ElementInputHandler::new(bounds, editor.clone()),
                                    app,
                                );
                            },
                        )
                        .size_full(),
                    ),
                )
        }
    }

    fn editor_probe_window(
        cx: &mut gpui::TestAppContext,
    ) -> (WindowHandle<EditorWindowProbe>, Entity<EditorInputProbe>, FocusHandle, FocusHandle) {
        let window = cx.update(|app| {
            app.open_window(WindowOptions::default(), |_, app| {
                let editor = app
                    .new(|app| EditorInputProbe { focus: app.focus_handle(), text: String::new() });
                app.new(|app| EditorWindowProbe {
                    editor,
                    terminal_focus: app.focus_handle(),
                    editor_keys: Vec::new(),
                    pty_bytes: Vec::new(),
                })
            })
            .unwrap()
        });
        let (editor, editor_focus, terminal_focus) = window
            .update(cx, |probe, _, app| {
                (
                    probe.editor.clone(),
                    probe.editor.read(app).focus.clone(),
                    probe.terminal_focus.clone(),
                )
            })
            .unwrap();
        (window, editor, editor_focus, terminal_focus)
    }

    fn focus_and_draw_probe(
        cx: &mut gpui::TestAppContext,
        window: gpui::AnyWindowHandle,
        focus: &FocusHandle,
    ) {
        cx.update_window(window, |_, window, app| {
            window.focus(focus, app);
            window.draw(app).clear();
        })
        .unwrap();
    }

    // @lat: [[test#Test Harness#GPUI Beads Editor Input Spike#Focused editor owns the key path]]
    #[gpui::test]
    fn focused_beads_editor_receives_text_without_leaking_keys_to_the_pty(
        cx: &mut gpui::TestAppContext,
    ) {
        let (window, editor, editor_focus, terminal_focus) = editor_probe_window(cx);

        focus_and_draw_probe(cx, window.into(), &editor_focus);
        assert!(
            cx.update_window(window.into(), |_, window, _| editor_focus.is_focused(window))
                .unwrap()
        );

        for key in ["x", "enter", "escape", "ctrl-c"] {
            cx.dispatch_keystroke(window.into(), gpui::Keystroke::parse(key).unwrap());
        }

        assert_eq!(editor.read_with(cx, |editor, _| editor.text.clone()), "x\n");
        window
            .update(cx, |probe, _, _| {
                assert_eq!(probe.editor_keys, ["x", "enter", "escape", "c"]);
                assert!(probe.pty_bytes.is_empty());
            })
            .unwrap();

        focus_and_draw_probe(cx, window.into(), &terminal_focus);
        cx.dispatch_keystroke(window.into(), gpui::Keystroke::parse("z").unwrap());
        window.update(cx, |probe, _, _| assert_eq!(probe.pty_bytes, [b"z".to_vec()])).unwrap();
    }

    // @lat: [[test#Test Harness#GPUI CI Run Bar#Owner action identities are region-scoped]]
    #[test]
    fn ci_action_ids_are_scoped_to_workspace() {
        let first = "00000000-0000-0000-0000-000000000001".parse().unwrap();
        let second = "10000000-0000-0000-0000-000000000001".parse().unwrap();
        let (first_open, first_dismiss) = ci_action_ids(first);
        let (second_open, second_dismiss) = ci_action_ids(second);

        assert_ne!(first_open, second_open);
        assert_ne!(first_dismiss, second_dismiss);
    }

    // @lat: [[test#Test Harness#GPUI CI Run Bar#CI controls retain keyboard focus]]
    #[test]
    fn ci_control_claim_prevents_terminal_focus_restore() {
        assert!(!focus_is_unclaimed([false, false, false, true, false]));
        assert!(focus_is_unclaimed([false; 5]));
    }

    // @lat: [[test#Test Harness#GPUI Beads Inline Editing#Armed editor survives terminal focus repair]]
    #[test]
    fn beads_editor_claim_prevents_terminal_focus_restore() {
        assert!(!focus_is_unclaimed([false, false, false, false, true]));
        assert!(focus_is_unclaimed([false; 5]));
    }

    // @lat: [[test#Test Harness#Terminal Client Singleton#Plain local launch owns the singleton]]
    #[test]
    fn terminal_singleton_only_applies_to_plain_local_launches() {
        assert!(terminal_singleton_required([false, false, false, false]));
        assert!(!terminal_singleton_required([true, false, false, false]));
        assert!(!terminal_singleton_required([false, true, false, false]));
        assert!(!terminal_singleton_required([false, false, true, false]));
        assert!(!terminal_singleton_required([false, false, false, true]));
    }

    // @lat: [[test#Test Harness#Terminal Client Singleton#Broker activation receipts stay ordered]]
    #[test]
    fn terminal_focus_broker_uses_activation_receipt_order() {
        let generation = scribe_common::socket::ClientFocusGeneration::new();
        let first_window = WindowId::new();
        let second_window = WindowId::new();
        let mut broker = TerminalFocusBroker::new();

        broker.record_restore_child(generation, first_window);
        broker.record_owner();
        broker.record_restore_child(generation, second_window);

        assert_eq!(broker.sequence(), 3);
        assert_eq!(
            broker.target(),
            TerminalFocusTarget::RestoreChild { generation, window_id: second_window }
        );
    }

    #[test]
    fn terminal_focus_broker_does_not_let_slow_auth_reorder_receipts() {
        let generation = scribe_common::socket::ClientFocusGeneration::new();
        let window_id = WindowId::new();
        let mut broker = TerminalFocusBroker::new();

        let child_receipt = broker.reserve_receipt();
        broker.record_owner();
        broker.record_restore_child_at(child_receipt, generation, window_id);

        assert_eq!(broker.sequence(), 2);
        assert_eq!(broker.target(), TerminalFocusTarget::Owner);
    }

    // @lat: [[test#Test Harness#Terminal Client Singleton#Owner replacement resets external recency]]
    #[test]
    fn terminal_focus_broker_owner_replacement_resets_external_recency() {
        let generation = scribe_common::socket::ClientFocusGeneration::new();
        let mut old_owner = TerminalFocusBroker::new();
        old_owner.record_restore_child(generation, WindowId::new());

        let replacement = TerminalFocusBroker::new();

        assert_eq!(replacement.sequence(), 0);
        assert_eq!(replacement.target(), TerminalFocusTarget::Owner);
    }

    // @lat: [[test#Test Harness#Terminal Client Singleton#Stale external winner falls back to the owner]]
    #[test]
    fn terminal_focus_broker_prunes_only_the_failed_winner_to_owner_fallback() {
        let first = scribe_common::socket::ClientFocusGeneration::new();
        let second = scribe_common::socket::ClientFocusGeneration::new();
        let window_id = WindowId::new();
        let mut broker = TerminalFocusBroker::new();
        broker.record_restore_child(first, window_id);

        assert!(!broker.prune_restore_child(second));
        assert_eq!(
            broker.target(),
            TerminalFocusTarget::RestoreChild { generation: first, window_id }
        );
        assert!(broker.prune_restore_child(first));
        assert_eq!(broker.target(), TerminalFocusTarget::Owner);
    }

    // @lat: [[test#Test Harness#Terminal Client Singleton#Restore-child activation acknowledgement reflects GPUI success]]
    #[test]
    fn restore_child_focus_endpoint_acknowledges_only_successful_activation() {
        use scribe_client::settings::singleton::{
            FocusEndpointRejection, FocusEndpointRequest, FocusEndpointResult,
        };

        let generation = scribe_common::socket::ClientFocusGeneration::new();

        assert_eq!(
            dispatch_focus_endpoint_request(
                &FocusEndpointRequest::Activate { generation },
                generation,
                || true,
            ),
            FocusEndpointResult::Activated { generation }
        );
        assert_eq!(
            dispatch_focus_endpoint_request(
                &FocusEndpointRequest::Activate { generation },
                generation,
                || false,
            ),
            FocusEndpointResult::Rejected { reason: FocusEndpointRejection::UnavailableWindow }
        );
    }

    // @lat: [[test#Test Harness#Terminal Client Singleton#Focus command reaches the owner]]
    #[test]
    fn terminal_singleton_listener_forwards_focus_commands() {
        use std::os::unix::net::{UnixListener, UnixStream};

        use scribe_client::settings::singleton;
        use scribe_common::settings_window::SettingsWindowCommand;

        let dir = std::env::temp_dir()
            .join(format!("scribe-client-focus-listener-{}", std::process::id()));
        drop(std::fs::create_dir_all(&dir));
        let socket_path = dir.join("client.sock");
        drop(std::fs::remove_file(&socket_path));
        let listener = UnixListener::bind(&socket_path).expect("test listener should bind");
        let focus = start_terminal_focus_listener(listener).expect("focus listener should start");
        let mut focus_rx = focus.focus_rx;

        let mut stream = UnixStream::connect(&socket_path).expect("focus sender should connect");
        singleton::write_command(&mut stream, &SettingsWindowCommand::focus(None))
            .expect("focus command should send");

        let deadline = Instant::now() + Duration::from_secs(1);
        let action = loop {
            if let Ok(action) = focus_rx.try_recv() {
                break action;
            }
            assert!(Instant::now() < deadline, "focus command should reach the GPUI receiver");
            std::thread::sleep(Duration::from_millis(5));
        };
        assert!(matches!(action, TerminalFocusAction::ActivateOwner));

        singleton::cleanup_socket(&socket_path);
        drop(std::fs::remove_dir_all(&dir));
    }

    // @lat: [[test#Test Harness#Server lifecycle#Graceful shutdown waits for every hosted window]]
    #[test]
    fn graceful_shutdown_waits_for_every_hosted_window() {
        let shutdown = ProcessShutdown::for_test();
        shutdown.register_view();
        shutdown.register_view();
        shutdown.request();

        assert!(shutdown.requested());
        assert!(!shutdown.finish_view());
        assert!(shutdown.finish_view());
    }

    // @lat: [[test#GPUI Workspace Dividers#Hover cursor follows split axis across hit band]]
    #[test]
    fn workspace_divider_cursor_follows_axis_across_hit_band() {
        assert_eq!(
            TerminalView::workspace_divider_cursor(SplitDirection::Horizontal),
            gpui::CursorStyle::ResizeLeftRight
        );
        assert_eq!(
            TerminalView::workspace_divider_cursor(SplitDirection::Vertical),
            gpui::CursorStyle::ResizeUpDown
        );

        let divider = workspace_layout::WorkspaceDivider {
            rect: Rect { x: 10.0, y: 20.0, width: 1.0, height: 100.0 },
            direction: SplitDirection::Horizontal,
            first_workspace: WorkspaceId::new(),
            second_workspace: WorkspaceId::new(),
            parent_rect: Rect { x: 0.0, y: 0.0, width: 200.0, height: 100.0 },
        };
        let hit_rect = workspace_layout::workspace_divider_hit_rect(&divider);
        assert!((hit_rect.x - 6.0).abs() < f32::EPSILON);
        assert!((hit_rect.y - 16.0).abs() < f32::EPSILON);
        assert!((hit_rect.width - 9.0).abs() < f32::EPSILON);
        assert!((hit_rect.height - 108.0).abs() < f32::EPSILON);
    }

    /// A `SessionInfo` carrying only what the reattach decision reads.
    fn listed_session(session_id: SessionId, provider: Option<AiProvider>) -> SessionInfo {
        SessionInfo {
            session_id,
            workspace_id: WorkspaceId::new(),
            launch_id: None,
            shell_name: String::from("bash"),
            title: None,
            icon_title: None,
            context: None,
            task_label: None,
            codex_task_label: None,
            cwd: None,
            git_branch: None,
            ai_state: None,
            ai_provider_hint: provider,
            prompt_state: None,
        }
    }

    #[test]
    fn session_list_restores_independent_title_sources() {
        let session = SessionId::new();
        let mut info = listed_session(session, None);
        info.title = Some(String::from("window"));
        info.icon_title = Some(String::from("icon"));

        let entry = tab_entry_for(&info);
        assert_eq!(entry.display_title(), "icon");
        assert_eq!(entry.terminal_title.as_deref(), Some("window"));
        assert_eq!(entry.icon_title.as_deref(), Some("icon"));

        info.icon_title = Some(String::new());
        assert_eq!(tab_entry_for(&info).display_title(), "window");
    }

    // @lat: [[client#Client#GPUI Client Spike#Hot Restart Reattach#Codex reattach defers its grid]]
    #[test]
    fn a_reattaching_codex_pane_announces_no_grid_and_no_resize() {
        let codex = SessionId::new();
        let shell = SessionId::new();
        let sessions =
            [listed_session(codex, Some(AiProvider::CodexCode)), listed_session(shell, None)];
        let retained = [codex, shell];
        let grids = [GridSize { cols: 100, rows: 30 }, GridSize { cols: 90, rows: 20 }];

        let panes = reattach_panes(&sessions, &retained, &grids, (8.0, 16.0));

        // Codex: nothing for the server to pre-size against, and no resize
        // riding in behind the attach to undo that.
        assert_eq!(panes[0].session_id, codex);
        assert_eq!(panes[0].size, TerminalSize::default());
        assert!(!panes[0].size.has_grid());
        assert!(!panes[0].resize_now);

        // Every other pane keeps the geometry it reattached with.
        assert_eq!(panes[1].session_id, shell);
        assert_eq!(
            panes[1].size,
            TerminalSize { cols: 90, rows: 20, cell_width: 8, cell_height: 16 }
        );
        assert!(panes[1].resize_now);
    }

    // @lat: [[lat.md/client#Client#GPUI Client Spike#Server Lifecycle Wiring]]
    #[test]
    fn fresh_window_bootstrap_is_claimed_exactly_once() {
        let bootstrap = InitialSessionBootstrap::new(true);

        assert!(bootstrap.claim(true, 0));
        assert!(!bootstrap.claim(true, 0));
    }

    // @lat: [[client#Client#GPUI Client Spike#Cold Restart Restore#An adopted window keeps its saved geometry]]
    #[test]
    fn a_window_opened_without_a_record_still_owes_the_assigned_one_a_read() {
        let seed = |restore_geometry| WindowSeed {
            terminal_size: TerminalSize { cols: 80, rows: 24, cell_width: 8, cell_height: 16 },
            restored: None,
            restore_siblings: 0,
            restore_geometry,
        };
        let mut record = WindowGeometry::default();
        record.x = Some(64);
        record.y = Some(48);
        record.width = 2000;
        record.height = 1200;
        record.monitor_name = Some("DP-1".to_owned());
        record.desktop = Some(3);

        // Opened at the default: no placement to reach, so its own bounds are
        // persisted — but the window the server assigns has yet to be read.
        let mut runtime = RestoreRuntime::from_seed(seed(None));
        assert_eq!(runtime.assigned_geometry, AssignedGeometry::Unread);
        assert_eq!(runtime.placement, RestorePlacement::Settled);

        // Reading it arms the same restore the seeded path uses, so the record
        // becomes the baseline instead of being written over.
        runtime.adopt_geometry_record(record.clone());
        assert_eq!(runtime.pending_position, Some(((64, 48), WindowState::Windowed)));
        assert_eq!(runtime.monitor.as_deref(), Some("DP-1"));
        assert_eq!(runtime.pending_desktop, Some(3));
        assert_eq!(runtime.geometry.as_ref(), Some(&record));

        // A windowed record has no `_NET_WM_STATE` atoms to assert, so nothing
        // is queued for the window state.
        assert_eq!(runtime.pending_state, None);

        // A window opened at its record named that window in `Hello`; there is
        // nothing left to adopt, and it starts out placing itself.
        let seeded = RestoreRuntime::from_seed(seed(Some(record)));
        assert_eq!(seeded.assigned_geometry, AssignedGeometry::Adopted);
        assert_eq!(seeded.placement, RestorePlacement::Restoring);
    }

    // @lat: [[client#Client#GPUI Client Spike#Cold Restart Restore#A maximized record is asserted, not toggled]]
    #[test]
    fn a_maximized_record_queues_the_state_for_assertion() {
        let seed = |restore_geometry| WindowSeed {
            terminal_size: TerminalSize { cols: 80, rows: 24, cell_width: 8, cell_height: 16 },
            restored: None,
            restore_siblings: 0,
            restore_geometry,
        };
        // Assigned field by field rather than with struct-update syntax: the
        // private legacy-`maximized` field blocks `..Default::default()`.
        let mut maximized = WindowGeometry::default();
        maximized.state = WindowState::Maximized;
        maximized.width = 3840;
        maximized.height = 2125;

        // Both halves of the restore queue it: the window opened at the record,
        // and the window that only learned which record it has afterwards. The
        // second is the one that has nothing else to fall back on — it was
        // never opened with `WindowBounds::Maximized` at all.
        let seeded = RestoreRuntime::from_seed(seed(Some(maximized.clone())));
        assert_eq!(seeded.pending_state, Some(WindowState::Maximized));
        let mut adopted = RestoreRuntime::from_seed(seed(None));
        adopted.adopt_geometry_record(maximized);
        assert_eq!(adopted.pending_state, Some(WindowState::Maximized));

        // Fullscreen owns an atom of its own and is queued the same way.
        let mut fullscreen = WindowGeometry::default();
        fullscreen.state = WindowState::Fullscreen;
        assert_eq!(
            RestoreRuntime::from_seed(seed(Some(fullscreen))).pending_state,
            Some(WindowState::Fullscreen)
        );

        // A minimized record is asserted into the state it unminimizes *into*,
        // not into minimization — `pending_minimize` owns that half, and a
        // window that comes back maximized-but-iconified has to carry both.
        let mut minimized = WindowGeometry::default();
        minimized.state = WindowState::Minimized;
        minimized.restore_state = WindowState::Maximized;
        let runtime = RestoreRuntime::from_seed(seed(Some(minimized)));
        assert_eq!(runtime.pending_minimize, Some(WindowState::Maximized));
        assert_eq!(runtime.pending_state, Some(WindowState::Maximized));
    }

    // @lat: [[client#Client#GPUI Client Spike#Cold Restart Restore#The state assert is verified and re-sent until it lands]]
    #[test]
    fn a_state_only_record_keeps_the_placement_open_until_the_assert_clears() {
        let seed = |restore_geometry| WindowSeed {
            terminal_size: TerminalSize { cols: 80, rows: 24, cell_width: 8, cell_height: 16 },
            restored: None,
            restore_siblings: 0,
            restore_geometry,
        };
        // A maximized record with no saved origin: nothing to move, only a
        // state to reach. Persisting the pre-assert reading here is exactly
        // the demotion that made one lost race permanent.
        let mut maximized = WindowGeometry::default();
        maximized.state = WindowState::Maximized;
        maximized.width = 2160;
        maximized.height = 3765;
        let mut runtime = RestoreRuntime::from_seed(seed(Some(maximized)));
        assert_eq!(runtime.pending_position, None);
        assert_eq!(runtime.placement, RestorePlacement::Restoring);

        // While the assert holds work — queued or in flight — captures are
        // adopted as baseline, never persisted.
        let in_flight =
            StateAssert { state: WindowState::Maximized, asked_at: Instant::now(), attempts: 1 };
        for target in [None, Some(in_flight)] {
            runtime.state_target = target;
            let pending = runtime.pending_state.is_some() || runtime.state_target.is_some();
            assert!(!runtime.placement.settled(pending));
            assert_eq!(runtime.placement, RestorePlacement::Restoring);
            runtime.pending_state = None;
        }

        // The assert clearing is what lets the placement converge, through the
        // same verify grace a position landing gets.
        runtime.state_target = None;
        assert!(!runtime.placement.settled(false));
        assert!(matches!(runtime.placement, RestorePlacement::Verifying(_)));
        if let RestorePlacement::Verifying(at) = &mut runtime.placement {
            *at = Instant::now().checked_sub(RESTORE_DEBOUNCE).unwrap();
        }
        assert!(runtime.placement.settled(false));
        assert_eq!(runtime.placement, RestorePlacement::Settled);
    }

    // @lat: [[test#Window geometry compat#X11 restore has one state transition]]
    #[cfg(target_os = "linux")]
    #[test]
    fn x11_creation_bounds_leave_window_state_to_the_assertion() {
        let bounds = Bounds::new(gpui::point(px(20.0), px(30.0)), size(px(800.0), px(600.0)));

        assert!(matches!(
            x11_creation_bounds(WindowBounds::Maximized(bounds)),
            WindowBounds::Windowed(_)
        ));
        assert!(matches!(
            x11_creation_bounds(WindowBounds::Fullscreen(bounds)),
            WindowBounds::Windowed(_)
        ));
        assert!(matches!(
            x11_creation_bounds(WindowBounds::Windowed(bounds)),
            WindowBounds::Windowed(_)
        ));
    }

    // @lat: [[lat.md/client#Client#GPUI Client Spike#Cold Restart Restore#Replayed prompts reach the live AI chrome]]
    #[test]
    fn restored_prompts_outlive_the_resumed_conversation_edge() {
        let mut ai = AiChrome::new(scribe_common::config::AiStateStylesConfig::default());
        let session = SessionId::new();
        let restored = PromptBarData::from(scribe_common::protocol::SessionPromptState {
            prompt_count: 3,
            first_prompt: Some("build the thing".to_owned()),
            latest_prompt: Some("now ship it".to_owned()),
            ..Default::default()
        });

        ai.restore_prompts(session, restored, Some("conv-42".to_owned()));
        // The resumed provider re-announces the id it was resumed with.
        ai.note_conversation(session, "conv-42");
        let kept = ai.prompts.get(&session).expect("restored prompts survive the resume edge");
        assert_eq!(kept.prompts.prompt_count, 3);
        assert_eq!(kept.prompts.first_prompt.as_deref(), Some("build the thing"));

        // A genuinely new conversation retires the previous one's bar.
        ai.note_conversation(session, "conv-43");
        assert!(!ai.prompts.contains_key(&session));
    }

    // @lat: [[client#GPUI Prompt Bar#A new conversation retires the bar and the meter]]
    #[test]
    fn a_new_conversation_retires_the_bar_and_the_meter() {
        let mut ai = AiChrome::new(scribe_common::config::AiStateStylesConfig::default());
        let session = SessionId::new();
        let at = std::time::SystemTime::UNIX_EPOCH;
        let edge = |conversation: &str, context| {
            let mut state = scribe_common::ai_state::AiProcessState::new(AiState::Processing);
            state.conversation_id = Some(conversation.to_owned());
            state.context = context;
            state
        };

        ai.apply_state_change(session, edge("conv-42", Some(80)), at);
        ai.record_prompt(session, "build the thing", at);
        assert!(ai.visible_prompts(session).is_some());
        assert_eq!(ai.tracker.context_for(session), Some(80));

        // The switching edge carries no fill of its own, because the server's
        // metadata merge stops at the conversation boundary.
        ai.apply_state_change(session, edge("conv-43", None), at);
        assert!(ai.visible_prompts(session).is_none(), "the retired rows go");
        assert_eq!(ai.tracker.context_for(session), None, "so does the retired percent");

        // The new conversation's own first reading is not collateral damage.
        ai.apply_state_change(session, edge("conv-43", Some(3)), at);
        assert_eq!(ai.tracker.context_for(session), Some(3));
    }

    // @lat: [[client#GPUI Prompt Bar#AI state edges freeze and resume the elapsed timer]]
    #[test]
    fn ai_state_edges_freeze_and_resume_the_elapsed_timer() {
        let mut ai = AiChrome::new(scribe_common::config::AiStateStylesConfig::default());
        let session = SessionId::new();
        let base = std::time::SystemTime::UNIX_EPOCH;
        let at = |secs: u64| base + std::time::Duration::from_secs(secs);
        // What the pane's strip would actually render at `now`.
        let shown = |chrome: &AiChrome, now| {
            prompt_bar::build_model(chrome.visible_prompts(session)?, now, None)?.elapsed_label
        };
        let edge = |state| scribe_common::ai_state::AiProcessState::new(state);

        // The prompt adapter emits state→processing before prompt_received.
        ai.apply_state_change(session, edge(AiState::Processing), at(100));
        ai.record_prompt(session, "build the thing", at(100));
        assert_eq!(shown(&ai, at(130)).as_deref(), Some("30 sec"), "a working AI ticks");

        // The AI stops 45s in: the figure holds there however long the pane sits.
        ai.apply_state_change(session, edge(AiState::IdlePrompt), at(145));
        assert_eq!(shown(&ai, at(200)).as_deref(), Some("45 sec"));
        assert_eq!(shown(&ai, at(100_000)).as_deref(), Some("45 sec"));

        // Repeated idle edges must not push the frozen figure forward.
        ai.apply_state_change(session, edge(AiState::WaitingForInput), at(400));
        assert_eq!(shown(&ai, at(100_000)).as_deref(), Some("45 sec"));

        // Back to work: the timer resumes from the original prompt instant.
        ai.apply_state_change(session, edge(AiState::Processing), at(500));
        assert_eq!(shown(&ai, at(520)).as_deref(), Some("7m 00s"));
    }

    // @lat: [[client#GPUI Client Spike#Hot Restart Reattach#Session list seeds the AI chrome]]
    #[test]
    fn session_list_seeds_the_prompt_bar_and_the_indicator() {
        let mut ai = AiChrome::new(scribe_common::config::AiStateStylesConfig::default());
        let session = SessionId::new();
        ai.clear(session);
        let mut ai_state = scribe_common::ai_state::AiProcessState::new_with_provider(
            scribe_common::ai_state::AiProvider::ClaudeCode,
            scribe_common::ai_state::AiState::Processing,
        );
        ai_state.conversation_id = Some("conv-42".to_owned());
        let info = SessionInfo {
            session_id: session,
            workspace_id: WorkspaceId::new(),
            launch_id: None,
            shell_name: String::from("bash"),
            title: None,
            icon_title: None,
            context: None,
            task_label: None,
            codex_task_label: None,
            cwd: None,
            git_branch: None,
            ai_state: Some(ai_state),
            ai_provider_hint: Some(scribe_common::ai_state::AiProvider::ClaudeCode),
            prompt_state: Some(scribe_common::protocol::SessionPromptState {
                prompt_count: 2,
                first_prompt: Some("build the thing".to_owned()),
                latest_prompt: Some("now ship it".to_owned()),
                latest_prompt_at: Some(1_700_000_000),
                latest_prompt_finished_at: None,
            }),
        };

        ai.seed_from_session_list(std::slice::from_ref(&info));

        let bar = ai.visible_prompts(session).expect("a reattached pane paints its prompt bar");
        assert_eq!(bar.prompts.prompt_count, 2);
        assert_eq!(bar.prompts.first_prompt.as_deref(), Some("build the thing"));
        assert_eq!(bar.prompts.latest_prompt_at, Some(1_700_000_000));
        assert_eq!(
            ai.tracker.provider_for_session(session),
            Some(scribe_common::ai_state::AiProvider::ClaudeCode),
            "the indicator comes back without waiting for a hook event"
        );
        assert!(!ai.binding_cleared.contains(&session));

        // The resumed provider re-announcing its own id is not a switch, so the
        // seeded bar survives the first live state edge.
        ai.note_conversation(session, "conv-42");
        assert!(ai.visible_prompts(session).is_some());
    }

    // @lat: [[client#Client#GPUI Client Spike#Hot Restart Reattach#Retained AI bindings stay structured]]
    #[test]
    fn retained_ai_metadata_builds_a_targeted_resume_binding() {
        let cwd = PathBuf::from("/tmp/paperclip");
        let binding = retained_session_binding(
            Some((AiProvider::CodexCode, Some("conv-42"))),
            Some(cwd.clone()),
            Some("launch-42".to_owned()),
        );

        assert_eq!(binding.launch_id, "launch-42");
        assert_eq!(binding.fallback_cwd, Some(cwd));
        assert!(matches!(
            binding.kind,
            LaunchKind::Ai {
                provider: AiProvider::CodexCode,
                resume_mode: AiResumeMode::Resume,
                conversation_id: Some(ref conversation_id),
            } if conversation_id == "conv-42"
        ));
    }

    // @lat: [[client#Client#GPUI Client Spike#Hot Restart Reattach#Live AI metadata updates restore]]
    #[test]
    fn live_ai_metadata_updates_restore_without_replacing_launch_identity() {
        let mut binding = restore_replay::new_shell_binding(None);
        let launch_id = binding.launch_id.clone();

        assert!(update_retained_binding(
            &mut binding,
            Some((AiProvider::CodexCode, Some("conv-42"))),
            Some(Path::new("/tmp/cue")),
        ));
        assert_eq!(binding.launch_id, launch_id);
        assert_eq!(binding.fallback_cwd.as_deref(), Some(Path::new("/tmp/cue")));
        assert!(matches!(
            binding.kind,
            LaunchKind::Ai {
                provider: AiProvider::CodexCode,
                resume_mode: AiResumeMode::Resume,
                conversation_id: Some(ref conversation_id),
            } if conversation_id == "conv-42"
        ));

        // Partial provider hooks do not erase the targeted resume id.
        assert!(!update_retained_binding(
            &mut binding,
            Some((AiProvider::CodexCode, None)),
            Some(Path::new("/tmp/cue")),
        ));

        // Only an explicit provider-exit edge demotes the binding.
        assert!(clear_retained_binding(&mut binding, Some(Path::new("/tmp/cue"))));
        assert_eq!(binding.launch_id, launch_id);
        assert!(matches!(binding.kind, LaunchKind::Shell));
    }

    #[test]
    fn restore_prompts_never_overwrites_a_live_prompt() {
        let mut ai = AiChrome::new(scribe_common::config::AiStateStylesConfig::default());
        let session = SessionId::new();
        // The replayed launch answered before the pane adopted the session, so
        // this prompt is newer than anything the snapshot holds.
        ai.record_prompt(session, "typed after the restart", std::time::SystemTime::now());

        ai.restore_prompts(
            session,
            PromptBarData::from(scribe_common::protocol::SessionPromptState {
                prompt_count: 9,
                ..Default::default()
            }),
            Some("conv-42".to_owned()),
        );

        assert_eq!(ai.prompts.get(&session).map(|data| data.prompts.prompt_count), Some(1));
    }

    #[test]
    fn restore_placement_suppresses_capture_until_the_landing_settles() {
        let mut placement = RestorePlacement::Restoring;

        // The move is still outstanding, and so is the landing check.
        assert!(!placement.settled(true));
        // The landing has just been checked and gets one more debounce.
        assert!(!placement.settled(false));
        assert!(matches!(placement, RestorePlacement::Verifying(_)));
        // Rewind past the debounce rather than sleeping through it.
        placement = RestorePlacement::Verifying(
            Instant::now().checked_sub(RESTORE_DEBOUNCE).expect("monotonic clock past the wait"),
        );
        assert!(placement.settled(false));
        assert!(placement.settled(false));
    }

    // @lat: [[client#GPUI Prompt Bar#Dismissal hides the strip without losing the history]]
    #[test]
    fn dismissed_prompt_bar_stays_down_until_the_session_is_forgotten() {
        let mut chrome = AiChrome::new(scribe_common::config::AiStateStylesConfig::default());
        let session = SessionId::new();
        chrome.apply_state_change(
            session,
            scribe_common::ai_state::AiProcessState::new(AiState::WaitingForInput),
            std::time::SystemTime::now(),
        );
        chrome.record_prompt(session, "build the thing", std::time::SystemTime::now());
        assert!(chrome.visible_prompts(session).is_some());

        chrome.dismiss(session);
        assert!(
            chrome.visible_prompts(session).is_none(),
            "a dismissed bar paints nothing and reserves no rows"
        );
        assert_eq!(
            chrome.prompts.get(&session).map(|data| data.prompts.prompt_count),
            Some(1),
            "the prompt history outlives the dismissal"
        );

        chrome.forget(session);
        assert!(!chrome.prompts.contains_key(&session), "session teardown clears the record");
        assert_eq!(
            chrome.tracker.provider_for_session(session),
            None,
            "the exited provider cannot keep split-scroll active"
        );
    }

    #[test]
    fn restore_placement_starts_settled_without_a_saved_position() {
        // A window that was never restoring must persist the size it came up at.
        assert!(RestorePlacement::Settled.settled(false));
    }

    // @lat: [[test#Test Harness#GPUI Client Headless Suites#Refused restore claim decision]]
    #[test]
    fn refused_restore_claim_is_dropped_before_session_list() {
        let claimed = WindowId::new();
        let assigned = WindowId::new();
        let snapshot = WindowRestoreState {
            version: 1,
            window_id: claimed,
            focused_workspace_id: WorkspaceId::new(),
            root: scribe_client::restore_state::WorkspaceLayoutSnapshot::Leaf {
                workspace_id: WorkspaceId::new(),
            },
            workspaces: Vec::new(),
            launches: Vec::new(),
        };
        let mut restore = RestoreRuntime::from_seed(WindowSeed {
            terminal_size: TerminalSize::default(),
            restored: Some(snapshot),
            restore_siblings: 2,
            restore_geometry: None,
        });

        // Welcome alone is enough to refuse the claim. No SessionList signal
        // exists on RestoreRuntime or enters this transition.
        restore.adopt_assigned_window(assigned);

        assert_eq!(restore.window_id, Some(assigned));
        assert!(restore.pending.is_none());
        assert!(restore.claimed_window.is_none());
        assert_eq!(restore.siblings, 0);
    }

    #[test]
    fn matching_restore_claim_waits_for_cold_or_warm_decision() {
        let claimed = WindowId::new();

        assert_eq!(
            restore_claim_disposition(Some(claimed), Some(claimed), true, 0),
            RestoreClaimDisposition::Cold,
        );
        assert_eq!(
            restore_claim_disposition(Some(claimed), Some(claimed), true, 1),
            RestoreClaimDisposition::Warm,
        );
    }

    #[test]
    fn existing_sessions_consume_fresh_window_bootstrap() {
        let bootstrap = InitialSessionBootstrap::new(true);

        assert!(!bootstrap.claim(true, 1));
        assert!(!bootstrap.claim(true, 0));
    }

    #[test]
    fn workspace_badge_palette_is_stable_for_name() {
        let palette = vec!["#112233".to_owned(), "#445566".to_owned()];
        let fallback = [1.0, 0.0, 0.0, 1.0];

        assert_eq!(
            TerminalView::workspace_badge_accent("scribe", &palette, fallback),
            TerminalView::workspace_badge_accent("scribe", &palette, fallback)
        );
    }

    #[test]
    fn workspace_badge_accent_comes_from_configured_palette() {
        let palette = vec!["#112233".to_owned(), "#445566".to_owned()];
        let selected =
            TerminalView::workspace_badge_accent("scribe", &palette, [1.0, 0.0, 0.0, 1.0]);

        assert!(palette.iter().any(|hex| {
            scribe_common::theme::hex_to_rgba(hex).is_ok_and(|rgba| opaque_slot(rgba) == selected)
        }));
    }

    #[test]
    fn workspace_badge_accent_falls_back_for_empty_or_invalid_palette() {
        let fallback = [0.25, 0.5, 0.75, 1.0];

        assert_eq!(
            TerminalView::workspace_badge_accent("scribe", &[], fallback),
            opaque_slot(fallback)
        );
        assert_eq!(
            TerminalView::workspace_badge_accent("scribe", &["invalid".to_owned()], fallback),
            opaque_slot(fallback)
        );
    }

    #[test]
    fn workspace_group_badge_requires_a_real_name() {
        let fallback = [0.25, 0.5, 0.75, 1.0];
        let badge = TerminalView::workspace_group_badge(Some(" scribe "), &[], fallback, false)
            .expect("named workspace has a badge");

        assert_eq!(badge.label, "scribe");
        assert!(TerminalView::workspace_group_badge(None, &[], fallback, false).is_none());
        assert!(TerminalView::workspace_group_badge(Some("  "), &[], fallback, false).is_none());
    }

    /// Build a plain key-down event for `key` with `modifiers` held.
    fn key_down(key: &str, modifiers: gpui::Modifiers) -> KeyDownEvent {
        KeyDownEvent {
            keystroke: gpui::Keystroke { modifiers, key: key.into(), key_char: None },
            is_held: false,
            prefer_character_input: false,
        }
    }

    // @lat: [[client#Input#Terminal focus keeps Tab for the PTY]]
    #[test]
    fn focused_terminal_keeps_tab_for_the_pty() {
        let tab = key_down("tab", gpui::Modifiers::default());
        assert!(!TerminalView::traversal_claims_tab(&tab, true));
        assert_eq!(encode_key(&tab).as_deref(), Some(b"\t".as_slice()));

        let shift = gpui::Modifiers { shift: true, ..gpui::Modifiers::default() };
        let shift_tab = key_down("tab", shift);
        assert!(!TerminalView::traversal_claims_tab(&shift_tab, true));
        assert_eq!(encode_key(&shift_tab).as_deref(), Some(b"\x1b[Z".as_slice()));
    }

    // @lat: [[client#Input#Chrome focus keeps Tab traversal]]
    #[test]
    fn chrome_focus_keeps_tab_traversal() {
        let tab = key_down("tab", gpui::Modifiers::default());
        assert!(TerminalView::traversal_claims_tab(&tab, false));

        let shift = gpui::Modifiers { shift: true, ..gpui::Modifiers::default() };
        assert!(TerminalView::traversal_claims_tab(&key_down("tab", shift), false));

        // Modified Tab chords stay with the bindings (`ctrl+tab` cycles panes),
        // and Ctrl+I — the same byte as Tab on the wire — is a different
        // keystroke that must never move focus.
        let ctrl = gpui::Modifiers { control: true, ..gpui::Modifiers::default() };
        assert!(!TerminalView::traversal_claims_tab(&key_down("tab", ctrl), false));
        let ctrl_i = key_down("i", ctrl);
        assert!(!TerminalView::traversal_claims_tab(&ctrl_i, false));
        assert_eq!(encode_key(&ctrl_i).as_deref(), Some(b"\t".as_slice()));
    }

    #[test]
    fn disabled_or_claimed_window_never_bootstraps_a_session() {
        // A cold restore replays its own panes, and a window claimed from the
        // server (a share join, a restored sibling) already has its sessions —
        // both are constructed disarmed, and nothing can re-arm them.
        let cold_restore = InitialSessionBootstrap::new(false);
        assert!(!cold_restore.claim(true, 0));
        assert!(!cold_restore.claim(true, 0));

        // A deliberate new window brings exactly one shell, once.
        let fresh = InitialSessionBootstrap::new(true);
        assert!(fresh.claim(true, 0));
        assert!(!fresh.claim(true, 0));
    }

    /// A snapshot whose rows each carry their own index, so a row that landed
    /// on the wrong line — or scrolled off into scrollback — is visible in the
    /// assertion rather than hidden behind identical filler.
    fn blank_screen_cell() -> ScreenCell {
        ScreenCell {
            c: ' ',
            fg: ScreenColor::Named(256),
            bg: ScreenColor::Named(257),
            flags: CellFlags::default(),
        }
    }

    fn numbered_snapshot(cols: u16, rows: u16) -> ScreenSnapshot {
        let mut cells = vec![blank_screen_cell(); usize::from(cols) * usize::from(rows)];
        for row in 0..usize::from(rows) {
            for (col, ch) in format!("row {row:02}").chars().enumerate() {
                cells[row * usize::from(cols) + col].c = ch;
            }
        }
        ScreenSnapshot {
            cells,
            cols,
            rows,
            cursor_col: 0,
            cursor_row: rows.saturating_sub(1),
            cursor_style: CursorStyle::Block,
            cursor_visible: true,
            alt_screen: false,
            active_dec_modes: Vec::new(),
            scrollback: Vec::new(),
            scrollback_rows: 0,
        }
    }

    /// Recreate the primary-screen prefix emitted before replay bytes became
    /// self-resetting. New clients prepend RIS before applying this form, then
    /// normalize the one blank history row its ED 2 creates.
    fn legacy_snapshot_bytes(snapshot: &ScreenSnapshot) -> Vec<u8> {
        let modern = scribe_common::screen_replay::snapshot_to_ansi(snapshot);
        let body = modern.strip_prefix(b"\x1bc\x1b[?25l\x1b[0m").expect("primary replay prefix");
        let mut legacy = b"\x1b[?25l\x1b[H\x1b[2J\x1b[0m".to_vec();
        legacy.extend_from_slice(body);
        session_lifecycle::ensure_replay_reset(legacy)
    }

    // @lat: [[client#Input#Resize Coordination#Rebuild applies at its own geometry]]
    #[gpui::test]
    fn rebuild_replays_at_the_geometry_it_was_rendered_at(_cx: &mut gpui::TestAppContext) {
        let session = SessionId::new();
        // Mid-drag the two sides disagree by a column: the client has already
        // narrowed its own grid and asked for the authoritative screen, and the
        // server answers from the width its `Term` still has.
        let mut grids = PaneGrids::new(119, 36);
        let pane = grids.pane(session);
        let snapshot = numbered_snapshot(120, 36);
        let op = PaneOp::Rebuild {
            bytes: scribe_common::screen_replay::snapshot_to_ansi(&snapshot),
            cols: snapshot.cols,
            rows: snapshot.rows,
            scrollback_rows: snapshot.scrollback_rows,
        };
        let prompt_marks = Arc::new(Mutex::new(PromptMarks::default()));

        pane.with_stream(|stream| apply_pane_op(&op, session, stream, &prompt_marks))
            .expect("pane stream applies the rebuild");

        let frame = pane.frame().expect("the pane republishes its projection");
        assert_eq!(frame.dimensions, (120, 36));
        assert_eq!(frame.metrics.history_size, 0);
        assert_eq!(pane.with_terminal(|terminal| terminal.scroll(Scroll::Delta(1))), Some(false));
        // Every snapshot row is on its own line: at the stale width each row
        // autowrapped, pushing the whole screen into scrollback and leaving a
        // blank viewport behind.
        for row in 0..36 {
            assert_eq!(frame.content.row_text(row).trim_end(), format!("row {row:02}"));
        }
    }

    #[gpui::test]
    fn rebuild_normalizes_legacy_zero_scrollback(_cx: &mut gpui::TestAppContext) {
        let session = SessionId::new();
        let snapshot = numbered_snapshot(80, 24);
        let op = PaneOp::Rebuild {
            bytes: legacy_snapshot_bytes(&snapshot),
            cols: snapshot.cols,
            rows: snapshot.rows,
            scrollback_rows: 0,
        };
        let mut grids = PaneGrids::new(80, 24);
        let pane = grids.pane(session);
        let prompt_marks = Arc::new(Mutex::new(PromptMarks::default()));

        pane.with_stream(|stream| apply_pane_op(&op, session, stream, &prompt_marks))
            .expect("pane stream applies the legacy rebuild");

        let frame = pane.frame().expect("the pane republishes its projection");
        assert_eq!(frame.metrics.history_size, 0);
        assert_eq!(pane.with_terminal(|terminal| terminal.scroll(Scroll::Delta(1))), Some(false));
    }

    // @lat: [[client#Input#Resize Coordination#Rebuild retains authoritative scrollback]]
    #[gpui::test]
    fn legacy_rebuild_retains_authoritative_scrollback(_cx: &mut gpui::TestAppContext) {
        let session = SessionId::new();
        let mut snapshot = numbered_snapshot(80, 24);
        snapshot.scrollback_rows = 3;
        snapshot.scrollback = vec![blank_screen_cell(); usize::from(snapshot.cols) * 3];
        let op = PaneOp::Rebuild {
            bytes: legacy_snapshot_bytes(&snapshot),
            cols: snapshot.cols,
            rows: snapshot.rows,
            scrollback_rows: snapshot.scrollback_rows,
        };
        let mut grids = PaneGrids::new(80, 24);
        let pane = grids.pane(session);
        let prompt_marks = Arc::new(Mutex::new(PromptMarks::default()));

        pane.with_stream(|stream| apply_pane_op(&op, session, stream, &prompt_marks))
            .expect("pane stream applies the rebuild");

        let frame = pane.frame().expect("the pane republishes its projection");
        assert_eq!(frame.metrics.history_size, 3);
        assert_eq!(pane.with_terminal(|terminal| terminal.scroll(Scroll::Delta(1))), Some(true));
        assert_eq!(pane.frame().unwrap().metrics.display_offset, 1);
    }

    #[gpui::test]
    fn legacy_scroll_bottom_keeps_a_scrolled_split_viewport(_cx: &mut gpui::TestAppContext) {
        let session = SessionId::new();
        let mut grids = PaneGrids::new(20, 8);
        let pane = grids.pane(session);
        let prompt_marks = Arc::new(Mutex::new(PromptMarks::default()));
        pane.with_terminal(|terminal| {
            terminal.feed_output(
                (1..=12)
                    .map(|row| format!("line {row:02}"))
                    .collect::<Vec<_>>()
                    .join("\r\n")
                    .as_bytes(),
            );
            assert!(terminal.scroll(Scroll::Delta(4)));
            assert!(terminal.set_split_scroll_eligibility(SplitScrollEligibility {
                scroll_pin_enabled: true,
                ai_provider_enabled: true,
            }));
        })
        .expect("pane accepts the scrolled split state");

        let redraws = pane
            .with_stream(|stream| {
                apply_pane_op(&PaneOp::ScrollBottom, session, stream, &prompt_marks)
            })
            .expect("pane accepts the legacy frame");

        assert_eq!(redraws, 0, "a legacy frame must not redraw a viewed viewport");
        pane.with_terminal(|terminal| {
            assert_eq!(terminal.display_offset(), 4, "the viewed anchor remains in place");
            assert!(terminal.pin_rows() > 0, "the split-scroll pin remains eligible");
            assert!(terminal.scroll(Scroll::Bottom), "an explicit bottom action still moves");
            assert_eq!(terminal.display_offset(), 0);
        })
        .expect("pane exposes the preserved viewport");
    }

    #[gpui::test]
    fn plain_ed3_output_keeps_the_terminal_clear_boundary(_cx: &mut gpui::TestAppContext) {
        let session = SessionId::new();
        let mut grids = PaneGrids::new(20, 8);
        let pane = grids.pane(session);
        let prompt_marks = Arc::new(Mutex::new(PromptMarks::default()));
        pane.with_terminal(|terminal| {
            terminal.feed_output(
                (1..=12)
                    .map(|row| format!("line {row:02}"))
                    .collect::<Vec<_>>()
                    .join("\r\n")
                    .as_bytes(),
            );
            assert!(terminal.scroll(Scroll::Delta(4)));
        })
        .expect("pane accepts scrollback");

        pane.with_stream(|stream| {
            apply_pane_op(&PaneOp::Output(b"\x1b[3J".to_vec()), session, stream, &prompt_marks)
        })
        .expect("pane applies the plain ED 3 output");

        assert_eq!(pane.frame().unwrap().metrics.display_offset, 0);
    }
}

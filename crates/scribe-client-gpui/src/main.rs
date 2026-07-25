//! One-pane GPUI client spike over Scribe's frozen local IPC protocol.

mod ipc_bridge;
mod session_lifecycle;
mod sync_frames;
mod terminal;
mod terminal_element;

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use gpui::{
    App, AsyncApp, Bounds, Context, Entity, FocusHandle, KeyDownEvent, MouseButton, MouseDownEvent,
    Point, Render, Subscription, Task, TitlebarOptions, WeakEntity, Window,
    WindowBackgroundAppearance, WindowBounds, WindowOptions, div, prelude::*, px, size,
};
use gpui_platform::application;
use scribe_client_gpui::ai_indicator::AiStateTracker;
use scribe_client_gpui::animation::AnimationSettings;
use scribe_client_gpui::chrome_metadata::{ChromeMetadata, SessionChrome};
use scribe_client_gpui::command_palette::{
    CommandPaletteColors, CommandPaletteEvent, CommandPaletteView, build_entries,
};
use scribe_client_gpui::config::{ConfigChangeSignal, ConfigReloadPlan, ConfigRuntime};
use scribe_client_gpui::context_menu::{
    ContextMenuColors, ContextMenuEvent, ContextMenuRequest, ContextMenuView,
};
use scribe_client_gpui::dialog::{
    AnyDialog, ClipboardDialog, CloseDialog, DialogColors, DialogEvent, DialogOutcome, DialogView,
    UpdateAction, UpdateDialogKind,
};
use scribe_client_gpui::input::KeyInput;
use scribe_client_gpui::keybindings::{KeyAction, LayoutAction, translate_key_action};
use scribe_client_gpui::layout::Rect;
use scribe_client_gpui::opacity::{clamp_opacity, opaque_slot, surface};
use scribe_client_gpui::prompt_bar::{
    self, PromptBarColors, PromptBarData, PromptContextIndicator,
};
use scribe_client_gpui::restore_replay::round_positive_f32_to_u16;
use scribe_client_gpui::share::{
    ControlRequestPrompt, ShareChrome, ShareKey, ShareKeyOutcome, ShareOverlayColors, ShareState,
    share_overlay,
};
use scribe_client_gpui::status_bar::{self, RemoteStatusData, StatusBarColors, StatusBarData};
use scribe_client_gpui::sys_stats::SystemStatsCollector;
use scribe_client_gpui::tooltip::{TooltipColors, TooltipPosition, TooltipRender, tooltip_element};
use scribe_client_gpui::update::UpdateState;
use scribe_client_gpui::workspace_notes::WorkspaceNoteEntry;
use scribe_client_gpui::workspace_notes_modal::{
    WorkspaceNotesModalAction, WorkspaceNotesModalColors, WorkspaceNotesModalView,
};
use scribe_client_gpui::x11_focus::X11FocusGuard;
use scribe_client_gpui::{
    tab_bar::{TabBarColors, context_suffix},
    tab_session::{TabEntry, TabSessions},
    titlebar::TitlebarView,
};
use scribe_common::ai_state::AiProvider;
use scribe_common::theme::ChromeColors;
use scribe_common::{
    config::{AiContextThresholds, StatusBarStatsConfig, load_config},
    framing::{read_message, write_message},
    ids::{SessionId, WorkspaceId},
    protocol::{
        ArchiveReason, ClientMessage, ServerMessage, SessionInfo, TerminalSize, WorkspaceNoteStatus,
    },
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
    sync_frames::{SyncFrameQueue, drain_all_committed},
    terminal::{Content, DisplayOnlyTerminal},
    terminal_element::{GridColors, GridFont, TerminalElement},
};

/// Wall-clock origin captured at the very top of `main`, used to time
/// startup-to-first-frame for the perf A/B rig (`tools/perf-ab-rig`).
static PROCESS_START: OnceLock<Instant> = OnceLock::new();

/// Latches once the first frame has emitted its startup-timing marker so the
/// per-frame `render` hook only measures the initial paint.
static FIRST_FRAME_LOGGED: AtomicBool = AtomicBool::new(false);

/// Env var that opts the client into the perf-rig startup marker. Its value is
/// a file path; when set to a non-empty path, the first `render` writes a
/// machine-parseable `first_frame_ms=<n>` marker to that file. The rig reads it
/// and gates it against the 500 ms Clarification-Q3 budget. Unset by default so
/// normal runs write nothing.
const STARTUP_TIMING_ENV: &str = "SCRIBE_GPUI_STARTUP_TIMING";

/// Writes the startup-to-first-frame marker exactly once, on the first painted
/// frame, when [`STARTUP_TIMING_ENV`] names an output file. The elapsed time is
/// measured from [`PROCESS_START`] so it captures the full window from process
/// launch through GPU-ready first paint, mirroring the old client's
/// `init_gpu_and_terminal_done` method that produced the recorded baseline.
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
    if let Err(error) = std::fs::write(&path, format!("first_frame_ms={elapsed_ms:.3}\n")) {
        tracing::warn!(%error, "failed to write startup-timing marker");
    }
}

const COLUMNS: u16 = 120;
const ROWS: u16 = 36;
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
    terminal: Arc<Mutex<DisplayOnlyTerminal>>,
    status: Arc<Mutex<String>>,
    generation: Arc<AtomicU64>,
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

/// The terminal grid's theme slots, kept in sRGB `[f32; 4]` theme space until
/// the render pass folds `appearance.opacity` into the background alpha.
#[derive(Debug, Clone, Copy, PartialEq)]
struct TerminalColors {
    background: [f32; 4],
    foreground: [f32; 4],
}

impl TerminalColors {
    /// Read the grid's colours out of a resolved theme.
    fn from_theme(theme: &scribe_common::theme::Theme) -> Self {
        Self { background: theme.background, foreground: theme.foreground }
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
    /// Font metrics the terminal grid paints with, rebuilt on a font reload.
    font: GridFont,
    /// Theme-derived prompt-bar palette, resolved once at view creation.
    prompt_colors: PromptBarColors,
    /// Warn/danger bands and per-band colours for the AI context-window meter.
    context_thresholds: AiContextThresholds,
    /// Whether `terminal.prompt_bar` is enabled in config.
    prompt_bar_enabled: bool,
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
    terminal_colors: TerminalColors,
    /// Live `appearance.opacity`, clamped to `0.0..=1.0`. Every background this
    /// window paints scales its alpha by this, so lowering it lets the desktop
    /// show through without recreating the (always transparent) window.
    opacity: f32,
    /// The command palette overlay, present only while open.
    command_palette: Option<Entity<CommandPaletteView>>,
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
    /// Demo toggle: when set, an OSC 8-style hover tooltip is drawn over a fixed
    /// anchor so the visual E2E can exercise tooltip clamping + URL truncation.
    tooltip_demo: bool,
    /// X11 active-window guard, present only when this window has an Xcb/Xlib
    /// window id (so: X11 sessions only). Suppresses keystrokes while a
    /// compositor overlay covers the window without sending a focus event.
    x11_focus: Option<X11FocusGuard>,
    // Held to keep the redraw poll alive; dropping the view cancels the task.
    _refresh_task: Task<()>,
    /// Held to keep the config-reload poll alive; dropping the view cancels it.
    _config_task: Task<()>,
    /// Held to keep the `_NET_ACTIVE_WINDOW` poll alive; dropping the view
    /// cancels it. `None` when the focus guard is not enabled.
    _x11_focus_task: Option<Task<()>>,
    /// Held to keep the window-activation observer alive, which clears the
    /// guard's reactivation debounce on a genuine focus event.
    _activation_observer: Option<Subscription>,
}

impl TerminalView {
    fn new(
        shared: Shared,
        sink: IpcSink,
        terminal_size: TerminalSize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // Start the X11 active-window guard from the live window: the Xcb/Xlib
        // window id only exists once the platform window has been created, and
        // `open_window` builds the root view with the real `Window` in hand.
        // Non-X11 backends yield no id, leaving `x11_focus` `None` and every
        // guard call below an explicit no-op.
        let x11_focus = X11FocusGuard::from_window_handle(&*window);
        let (x11_focus_task, activation_observer) = if x11_focus.is_some() {
            tracing::info!(
                window = scribe_client_gpui::x11_focus::xcb_window_id(&*window),
                "X11 active-window guard enabled"
            );
            let task = cx.spawn(async move |view, app| drive_x11_focus_polls(view, app).await);
            let observer =
                cx.observe_window_activation(window, |view, window, _| view.on_activation(window));
            (Some(task), Some(observer))
        } else {
            (None, None)
        };
        let generation = Arc::clone(&shared.generation);
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
        let terminal_colors = TerminalColors::from_theme(theme);
        let chrome = config.config().chrome;
        let opacity = clamp_opacity(config.opacity());
        let font = GridFont::from_appearance(&config.config().config.appearance);
        let stats_config = config.config().config.terminal.status_bar_stats.clone();
        let terminal = &config.config().config.terminal;
        let context_thresholds = terminal.ai_session.context_thresholds.clone();
        let prompt_bar_enabled = terminal.prompt_bar.enabled;
        let prompt_colors = PromptBarColors::from(&chrome);
        let colors = TabBarColors::from_chrome(&chrome, opacity);
        // The strip starts empty and is filled by the reader's first
        // `SessionList`; `sync_tabs` pushes it into the titlebar on the next
        // redraw so the tab row always mirrors live server state.
        let rendered_tabs = TabSessions::new();
        let titlebar = cx.new(|cx| {
            let mut bar = TitlebarView::new(colors, cx);
            bar.set_tabs(rendered_tabs.to_tab_data(), cx);
            bar
        });
        Self {
            shared,
            sink,
            focus_handle: cx.focus_handle(),
            config,
            stats: SystemStatsCollector::new(),
            status_colors,
            stats_config,
            font,
            prompt_colors,
            context_thresholds,
            prompt_bar_enabled,
            titlebar,
            chrome,
            terminal_size,
            rendered_tabs,
            terminal_colors,
            opacity,
            command_palette: None,
            context_menu: None,
            dialog: None,
            update_dialog_kind: None,
            workspace_notes_modal: None,
            tooltip_demo: false,
            x11_focus,
            _refresh_task: refresh_task,
            _config_task: config_task,
            _x11_focus_task: x11_focus_task,
            _activation_observer: activation_observer,
        }
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
            self.terminal_colors = TerminalColors::from_theme(&theme);
            self.chrome = theme.chrome;
            self.prompt_colors = PromptBarColors::from(&self.chrome);
            self.push_tab_bar_colors(cx);
            // Open overlays captured the old palette when they were built; drop
            // them so a live theme edit never leaves stale colours on screen.
            self.command_palette = None;
            self.context_menu = None;
        }

        if plan.font_changed() {
            self.font = GridFont::from_appearance(&self.config.config().config.appearance);
            self.report_cell_metrics();
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

    /// Republish the cell grid's size after a font edit changed cell metrics.
    fn report_cell_metrics(&self) {
        let Some(session_id) = self.shared.active_session.lock().ok().and_then(|guard| *guard)
        else {
            return;
        };
        let size = TerminalSize {
            cols: COLUMNS,
            rows: ROWS,
            cell_width: round_positive_f32_to_u16(self.font.cell_width()).max(1),
            cell_height: round_positive_f32_to_u16(self.font.line_height).max(1),
        };
        if let Err(error) = self.sink.resize(session_id, size) {
            tracing::warn!(%error, "resize after font reload dropped: IPC writer closed");
        }
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
    /// Tab creation, selection, and closing are wired to the IPC sink; the
    /// pane/workspace/navigation families are still swallowed (never forwarded
    /// to the PTY) because the shell has no pane tree yet, matching the legacy
    /// client's behaviour of never leaking a bound shortcut as terminal bytes.
    ///
    /// The match is exhaustive and the swallowed variants are named one by one
    /// rather than folded into a `_` arm: a new [`LayoutAction`] then fails to
    /// compile here instead of silently joining the dropped set, and every drop
    /// that does happen is counted and warned by [`unhandled_layout_action`].
    fn handle_layout_action(&mut self, action: LayoutAction, cx: &mut Context<Self>) {
        match action {
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
            LayoutAction::SplitVertical
            | LayoutAction::SplitHorizontal
            | LayoutAction::ClosePane
            | LayoutAction::FocusNext
            | LayoutAction::FocusLeft
            | LayoutAction::FocusRight
            | LayoutAction::FocusUp
            | LayoutAction::FocusDown
            | LayoutAction::WorkspaceSplitVertical
            | LayoutAction::WorkspaceSplitHorizontal
            | LayoutAction::WorkspaceFocusLeft
            | LayoutAction::WorkspaceFocusRight
            | LayoutAction::WorkspaceFocusUp
            | LayoutAction::WorkspaceFocusDown
            | LayoutAction::NewWindow
            | LayoutAction::CopySelection
            | LayoutAction::PasteClipboard
            | LayoutAction::ScrollUp
            | LayoutAction::ScrollDown
            | LayoutAction::ScrollTop
            | LayoutAction::ScrollBottom
            | LayoutAction::PromptJumpUp
            | LayoutAction::PromptJumpDown
            | LayoutAction::JumpToFailure
            | LayoutAction::ZoomIn
            | LayoutAction::ZoomOut
            | LayoutAction::ZoomReset => unhandled_layout_action(action),
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
            self.sink.create_session(workspace_id, self.terminal_size, None, command)
        {
            tracing::warn!(%error, "new tab dropped: IPC writer closed");
        }
    }

    /// Move the tab selection with `move_selection` and attach whatever it
    /// lands on. A `None` result means the selection did not move.
    fn switch_tab(
        &mut self,
        move_selection: impl FnOnce(&mut TabSessions) -> Option<SessionId>,
        cx: &mut Context<Self>,
    ) {
        let Ok(mut tabs) = self.shared.tabs.lock() else { return };
        let Some(session_id) = move_selection(&mut tabs) else { return };
        drop(tabs);
        self.attach(session_id);
        self.sync_tabs(cx);
    }

    /// Point the client at `session_id`: attach it, announce the client size,
    /// and ask for a fresh screen so the switched-to tab paints immediately.
    fn attach(&self, session_id: SessionId) {
        if let Ok(mut guard) = self.shared.active_session.lock() {
            *guard = Some(session_id);
        }
        let size = self.terminal_size;
        let result = self
            .sink
            .attach_sessions(vec![session_id], vec![size])
            .and_then(|()| self.sink.resize(session_id, size));
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
        }
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
        cx.subscribe(&palette, |this, _palette, event: &CommandPaletteEvent, ctx| match event {
            CommandPaletteEvent::Execute(_) | CommandPaletteEvent::Dismissed => {
                this.command_palette = None;
                ctx.notify();
            }
        })
        .detach();
        self.command_palette = Some(palette);
        cx.notify();
    }

    /// Open the right-click context menu at `position` with a representative item
    /// set (selection + OSC 8 URL), subscribing so a choice or dismiss closes it.
    fn open_context_menu(&mut self, position: Point<gpui::Pixels>, cx: &mut Context<Self>) {
        self.command_palette = None;
        let colors = ContextMenuColors::from(&self.chrome);
        let request = ContextMenuRequest {
            has_selection: true,
            osc8_uri: Some("https://example.com/spec".into()),
            ..Default::default()
        };
        let menu = cx.new(|cx| ContextMenuView::new(colors, request, position, cx));
        cx.subscribe(&menu, |this, _menu, event: &ContextMenuEvent, ctx| match event {
            ContextMenuEvent::Selected(_) | ContextMenuEvent::Dismissed => {
                this.context_menu = None;
                ctx.notify();
            }
        })
        .detach();
        self.context_menu = Some(menu);
        cx.notify();
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
            if let DialogOutcome::Update(action) = outcome {
                this.route_update_action(*action);
            }
            this.dialog = None;
            this.update_dialog_kind = None;
            ctx.notify();
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
        let overlay_free = self.dialog.is_none() && self.workspace_notes_modal.is_none();

        // Config-driven shortcuts are matched against the live bindings, which
        // the config watcher re-parses on every reload — so a saved keybinding
        // edit takes effect on the very next keystroke, with no restart. Only
        // the palette intercept is claimed here; every other translation falls
        // through to the demo keys and the terminal encoder below.
        if overlay_free {
            let opens_palette = KeyInput::from_key_down(event).is_some_and(|input| {
                matches!(
                    translate_key_action(&input, self.config.bindings()),
                    Some(KeyAction::OpenCommandPalette)
                )
            });
            if opens_palette {
                self.open_command_palette(cx);
                return true;
            }
        }

        // Ctrl+Shift+U toggles the tooltip demo; Ctrl+Shift+Q opens the close
        // dialog; Ctrl+Shift+K opens the clipboard dialog; Ctrl+Shift+N opens
        // the workspace-notes modal.
        if mods.control && mods.shift && overlay_free {
            match event.keystroke.key.as_str() {
                "u" => {
                    self.tooltip_demo = !self.tooltip_demo;
                    cx.notify();
                    return true;
                }
                "q" => {
                    self.open_dialog(AnyDialog::Close(CloseDialog::new(1)), cx);
                    return true;
                }
                "k" => {
                    self.open_dialog(
                        AnyDialog::Clipboard(ClipboardDialog::new(
                            scribe_common::protocol::PromptId(1),
                            scribe_common::protocol::ClipboardOp::Write,
                            scribe_common::protocol::ClipboardSelection::Clipboard,
                            Some("export TOKEN=hunter2".to_owned()),
                        )),
                        cx,
                    );
                    return true;
                }
                "n" => {
                    self.open_workspace_notes_modal(cx);
                    return true;
                }
                _ => {}
            }
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
        match action {
            KeyAction::Layout(layout) => self.handle_layout_action(layout, cx),
            KeyAction::Terminal(bytes) => self.send_key_bytes(bytes),
            // The palette is claimed earlier by `handle_overlay_key`; the
            // settings window and find overlay are separate beads. All three
            // are still swallowed so they cannot reach the PTY.
            KeyAction::OpenCommandPalette | KeyAction::OpenSettings | KeyAction::OpenFind => {
                tracing::debug!(?action, "key action not yet wired in the GPUI shell");
            }
        }
        true
    }

    /// Handle a window activation change from the platform.
    ///
    /// Compositor overlays never send focus events, so a genuine activation
    /// means the user really is back: drop the guard's reactivation debounce
    /// rather than eating the keystroke that follows a real refocus.
    fn on_activation(&mut self, window: &Window) {
        if !window.is_window_active() {
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

    /// Enqueue already-encoded bytes for the attached pane.
    fn send_key_bytes(&self, bytes: Vec<u8>) {
        let session_id = self.shared.active_session.lock().ok().and_then(|guard| *guard);
        let Some(session_id) = session_id else {
            return;
        };
        if let Err(error) = self.sink.key_input(session_id, bytes, true) {
            tracing::warn!(%error, "dropped keystroke: IPC writer closed");
        }
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

/// Interim keystroke encoder feeding the outbound [`IpcSink`] (see
/// [`TerminalView::on_key_down`]).
fn encode_key(event: &KeyDownEvent) -> Option<Vec<u8>> {
    if let Some(text) = event.keystroke.key_char.as_ref()
        && !text.is_empty()
    {
        return Some(text.clone().into_bytes());
    }
    let bytes: &[u8] = match event.keystroke.key.as_str() {
        "enter" => b"\r",
        "tab" => b"\t",
        "backspace" => b"\x7f",
        "escape" => b"\x1b",
        "up" => b"\x1b[A",
        "down" => b"\x1b[B",
        "right" => b"\x1b[C",
        "left" => b"\x1b[D",
        _ => return None,
    };
    Some(bytes.to_vec())
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
                remote: RemoteStatusData { enabled: false, controllers: &[] },
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
            24.,
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
        if !self.focus_handle.is_focused(window) {
            window.focus(&self.focus_handle, cx);
        }
        self.sync_tabs(cx);
        let content = self
            .shared
            .terminal
            .lock()
            .map_or_else(|_| Content::default(), |guard| guard.content());
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
        let grid_colors = GridColors {
            background: surface(self.terminal_colors.background, opacity),
            foreground: opaque_slot(self.terminal_colors.foreground),
        };
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
                if view.handle_overlay_key(event, ctx) || view.handle_binding(event, ctx) {
                    return;
                }
                view.on_key_down(event, ctx);
            }))
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .child(self.titlebar.clone())
            .child(
                // Wrap the terminal grid so a right-click opens the context menu
                // at the cursor without disturbing the display-only element.
                div()
                    .flex_1()
                    .child(TerminalElement::new(content, self.font.clone(), grid_colors).paint())
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(|view, event: &MouseDownEvent, _window, ctx| {
                            view.open_context_menu(event.position, ctx);
                        }),
                    ),
            )
            .children(prompt_strip)
            .child(
                div()
                    .h(px(26.))
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

fn main() {
    PROCESS_START.get_or_init(Instant::now);
    init_tracing();

    // `scribe-client --settings` opens (or focuses) the settings window instead
    // of the terminal shell. The singleton absorbs the old scribe-settings
    // `settings.lock`/`settings.sock`: a second launch hands focus to the
    // running window and exits here.
    if std::env::args().skip(1).any(|arg| arg == "--settings") {
        run_settings();
        return;
    }

    let shared = Shared {
        terminal: Arc::new(Mutex::new(DisplayOnlyTerminal::new(
            usize::from(COLUMNS),
            usize::from(ROWS),
        ))),
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
    };
    let terminal_size = TerminalSize {
        cols: COLUMNS,
        rows: ROWS,
        cell_width: CELL_WIDTH,
        cell_height: CELL_HEIGHT,
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

    application().run(move |cx: &mut App| {
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
    let bounds = Bounds::centered(None, size(px(960.), px(680.)), cx);
    let shared = shared.clone();
    let sink = sink.clone();
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
        |window, cx| cx.new(|cx| TerminalView::new(shared, sink, terminal_size, window, cx)),
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

async fn run_connection(ctx: IpcThread) -> Result<(), String> {
    let stream = tokio::net::UnixStream::connect(server_socket_path())
        .await
        .map_err(|error| error.to_string())?;
    // Socket is up: light the status-bar connection dot green.
    ctx.shared.connected.store(true, Ordering::Release);
    let (reader, writer) = stream.into_split();

    // Handshake is queued ahead of any sink traffic on the same ordered channel.
    ctx.out_tx
        .send(ClientMessage::Hello { window_id: None, clipboard_gating: false, takeover: false })
        .map_err(|_| "writer channel closed".to_owned())?;
    ctx.out_tx.send(ClientMessage::ListSessions).map_err(|_| "writer channel closed".to_owned())?;

    tokio::spawn(run_writer(writer, ctx.out_rx));
    spawn_drain(ctx.in_rx, Arc::clone(&ctx.shared.terminal), Arc::clone(&ctx.shared.generation));

    run_reader(
        reader,
        ReaderCtx {
            status: ctx.shared.status,
            generation: ctx.shared.generation,
            active_session: ctx.shared.active_session,
            ai: ctx.shared.ai,
            chrome_metadata: ctx.shared.chrome_metadata,
            tabs: ctx.shared.tabs,
            share: ctx.shared.share,
            update: ctx.shared.update,
            out_tx: ctx.out_tx,
            in_tx: ctx.in_tx,
            sink: ctx.sink,
            size: ctx.size,
        },
    )
    .await
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
    terminal: Arc<Mutex<DisplayOnlyTerminal>>,
    generation: Arc<AtomicU64>,
) {
    let queues: SyncFrameQueues = Arc::new(Mutex::new(HashMap::new()));
    let expiry_wake = Arc::new(Notify::new());

    let queue_task = Arc::clone(&queues);
    let terminal_task = Arc::clone(&terminal);
    let generation_task = Arc::clone(&generation);
    let wake_task = Arc::clone(&expiry_wake);
    tokio::spawn(run_drain(in_rx, move |batch| {
        if batch.is_empty() {
            return;
        }
        let mut redraws = 0usize;
        let mut sync_armed = false;
        if let (Ok(mut session_queues), Ok(mut guard)) = (queue_task.lock(), terminal_task.lock()) {
            for (session, bytes) in batch.iter() {
                let queue = session_queues.entry(session).or_default();
                queue.queue_output_frames(bytes);
                redraws += usize::from(drain_all_committed(queue, &mut *guard).needs_redraw);
                sync_armed |= queue.raw_sync_deadline().is_some();
            }
            sync_armed |= guard.parser_sync_deadline().is_some();
        }
        for _ in 0..redraws {
            generation_task.fetch_add(1, Ordering::Release);
        }
        if sync_armed {
            wake_task.notify_one();
        }
    }));

    tokio::spawn(run_sync_expiry(queues, terminal, generation, expiry_wake));
}

/// Waits on the nearest synchronized-update deadline and commits it once it
/// expires, so an unterminated `CSI ? 2026 h` still flushes after 150 ms even
/// while the inbound channel is idle. When nothing is buffering, it parks until
/// the drain task arms a fresh deadline.
async fn run_sync_expiry(
    queues: SyncFrameQueues,
    terminal: Arc<Mutex<DisplayOnlyTerminal>>,
    generation: Arc<AtomicU64>,
    wake: Arc<Notify>,
) {
    loop {
        match next_sync_deadline(&queues, &terminal) {
            None => wake.notified().await,
            Some(deadline) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                tokio::select! {
                    () = tokio::time::sleep(remaining) => flush_expired_sync(&queues, &terminal, &generation),
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
    terminal: &Arc<Mutex<DisplayOnlyTerminal>>,
    generation: &Arc<AtomicU64>,
) {
    let now = Instant::now();
    let mut redraws = 0usize;
    if let (Ok(mut session_queues), Ok(mut guard)) = (queues.lock(), terminal.lock()) {
        redraws += usize::from(guard.flush_parser_sync_timeout(now));
        for queue in session_queues.values_mut() {
            let flushed = queue.flush_raw_timeout(now)
                && drain_all_committed(queue, &mut *guard).needs_redraw;
            redraws += usize::from(flushed);
        }
    }
    for _ in 0..redraws {
        generation.fetch_add(1, Ordering::Release);
    }
}

/// Nearest synchronized-update deadline across every pane's raw-frame queue and
/// the shared parser, or `None` when nothing is buffering.
fn next_sync_deadline(
    queues: &SyncFrameQueues,
    terminal: &Arc<Mutex<DisplayOnlyTerminal>>,
) -> Option<Instant> {
    let parser = terminal.lock().ok().and_then(|guard| guard.parser_sync_deadline());
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
    let refocused = ctx.tabs.lock().ok().and_then(|mut tabs| tabs.remove(session_id));
    if let Some(next) = refocused {
        attach_session(ctx, next)?;
    }
    if existed && Some(session_id) == attached {
        set_status(&ctx.status, &ctx.generation, "attached pane exited".to_owned());
    }
    Ok(())
}

/// Decode a reattach replay onto the pane, or surface the decode failure.
///
/// A corrupt reattach stream must not tear down the reader: show an error on
/// the pane and keep the connection alive.
fn forward_replay(ctx: &ReaderCtx, session_id: SessionId, replay: &SessionReplay) {
    match session_lifecycle::decode_replay(session_id, replay) {
        Ok(bytes) => forward_output(&ctx.in_tx, session_id, bytes),
        Err(error) => set_status(&ctx.status, &ctx.generation, error.to_string()),
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
            // A takeover Hello's Welcome hands back the adopted window id, and
            // the additive v3 field names this connection's own share seat so a
            // later roster matches it exactly instead of by device name.
            registry.adopt_window(window_id);
            update_share_chrome(ctx, |share| share.set_self_id(participant_id));
            tracing::debug!(
                adopted = ?registry.adopted_window(),
                ?participant_id,
                "welcome: adopted window"
            );
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
        ServerMessage::PtyOutput { session_id, data } => {
            if Some(session_id) == attached {
                forward_output(&ctx.in_tx, session_id, data);
            }
        }
        ServerMessage::SessionReplay { session_id, replay } => {
            if Some(session_id) == attached {
                forward_replay(ctx, session_id, &replay);
            }
        }
        ServerMessage::ScreenSnapshot { session_id, snapshot } => {
            if Some(session_id) == attached {
                // Reset before replaying so the tooling snapshot replaces,
                // never appends onto, the pane's current content.
                let bytes = session_lifecycle::snapshot_reset_bytes(&snapshot);
                forward_output(&ctx.in_tx, session_id, bytes);
            }
        }
        // The terminal-chrome family all lands in the same two stores (the tab
        // strip's labels and the shared metadata), so it is named here and
        // routed as one to [`on_chrome_message`].
        chrome @ (ServerMessage::TitleChanged { .. }
        | ServerMessage::CwdChanged { .. }
        | ServerMessage::GitBranch { .. }
        | ServerMessage::SessionContextChanged { .. }
        | ServerMessage::EnvStatus { .. }
        | ServerMessage::WorkspaceNamed { .. }) => on_chrome_message(ctx, chrome),
        // Both update broadcasts land in the same shared state behind the
        // centred status-bar CTA, so they are named here and routed as one to
        // [`on_update_message`].
        update @ (ServerMessage::UpdateAvailable { .. } | ServerMessage::UpdateProgress { .. }) => {
            on_update_message(ctx, update);
        }
        ServerMessage::Error { message } => set_status(&ctx.status, &ctx.generation, message),
        // Feature 015: the four share/control notices are routed as one group so
        // this table stays a screen of routing decisions; the arm names each
        // variant, so none of them can fall through to the drop counter.
        share @ (ServerMessage::ShareRoster { .. }
        | ServerMessage::ControlRequested { .. }
        | ServerMessage::ControlDenied { .. }
        | ServerMessage::ShareEnded { .. }) => dispatch_share_message(share, ctx),
        other => unhandled_server_message(&other),
    }
    Ok(())
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
    let entry = TabEntry { session_id, workspace_id, title: shell_name };
    let added = ctx.tabs.lock().is_ok_and(|mut tabs| tabs.insert_active(entry));
    if added {
        attach_session(ctx, session_id)?;
        set_status(&ctx.status, &ctx.generation, "opened a new tab".to_owned());
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
/// to the shell basename, matching what the legacy tab bar rendered.
fn tab_entry_for(info: &SessionInfo) -> TabEntry {
    TabEntry {
        session_id: info.session_id,
        workspace_id: info.workspace_id,
        title: info.title.clone().unwrap_or_else(|| info.shell_name.clone()),
    }
}

/// Attach `session_id` and make it the pane the view renders and types into.
///
/// The server answers with a `SessionReplay` for the newly attached session,
/// which repaints the pane, so no extra snapshot request is needed.
fn attach_session(ctx: &ReaderCtx, session_id: SessionId) -> Result<(), String> {
    ctx.out_tx
        .send(ClientMessage::AttachSessions {
            session_ids: vec![session_id],
            dimensions: vec![ctx.size],
        })
        .map_err(|_| "writer channel closed".to_owned())?;
    // Announce the client size through the sink, ahead of any KeyInput.
    ctx.sink.resize(session_id, ctx.size).map_err(|error| error.to_string())?;
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

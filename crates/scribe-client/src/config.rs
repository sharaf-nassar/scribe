//! Config loading, live-reload, and theme consumption for the GPUI client.
//!
//! This ports the config watcher and runtime-reload semantics from
//! `crates/scribe-client/src/config.rs` and the reload bookkeeping in the
//! legacy client's `main.rs`, retargeted at the GPUI paint path. It reuses the
//! frozen `scribe-common` config surface end to end — [`ScribeConfig`],
//! [`load_config`], [`resolve_theme`], and [`ChromeColors`] — so the TOML
//! format, flavor-specific config directories, inline `[theme]` handling, and
//! removed-key tolerance all stay byte-for-byte identical to the old client.
//!
//! The [`ClientConfig`] snapshot bundles the parsed config, resolved
//! [`Theme`]/[`ChromeColors`], and parsed [`Bindings`]. [`ClientConfig::reload`]
//! swaps in a freshly loaded config and returns a [`ConfigReloadPlan`] naming
//! which live surfaces (theme, font metrics, opacity) must be reapplied — so a
//! saved edit to theme, font, or keybindings takes effect without a restart.
//!
//! [`ConfigRuntime`] is the piece the terminal window actually owns: it holds
//! the snapshot, keeps the `notify` watcher alive, and hands the foreground a
//! [`ConfigChangeSignal`] it can poll from a GPUI task. The watcher callback
//! runs on notify's own thread and must never touch GPUI state, so it only
//! bumps an atomic generation; the UI thread drains it with
//! [`ConfigRuntime::poll_reload`] and reapplies the returned plan.

use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use scribe_common::app::current_config_dir;
use scribe_common::config::{ScribeConfig, load_config, resolve_theme};
use scribe_common::theme::{ChromeColors, Theme};

use crate::keybindings::Bindings;

pub use notify::RecommendedWatcher;
use notify::{RecursiveMode, Watcher};

/// Return whether a notify path should trigger a config reload.
///
/// We normally care only about `config.toml` or files inside `themes/`. On
/// macOS, `notify` uses `FSEvents`, which can report only the watched directory
/// and expects clients to rescan it, so the root config dir itself must also
/// count as relevant there.
#[must_use]
pub fn is_relevant_config_event_path(config_dir: &Path, path: &Path) -> bool {
    path.file_name().is_some_and(|name| name == "config.toml")
        || path.components().any(|component| component.as_os_str() == "themes")
        || (cfg!(target_os = "macos") && path == config_dir)
}

/// Start a file watcher on the active (flavor-specific) scribe config directory.
///
/// Watches the whole config directory (not just the file) because editors often
/// delete + recreate files on save. `on_change` is invoked once per relevant
/// modify/create event — the GPUI caller forwards it to the UI thread to run
/// [`ClientConfig::reload`]. The relevance filter matches the legacy client via
/// [`is_relevant_config_event_path`].
///
/// Returns the watcher handle. **The caller must store this** — dropping it
/// stops the watcher.
#[must_use]
pub fn start_config_watcher<F>(on_change: F) -> Option<RecommendedWatcher>
where
    F: Fn() + Send + 'static,
{
    let config_path = current_config_dir()?;
    let watched_config_dir = config_path.clone();

    let mut watcher = notify::recommended_watcher(move |res: Result<notify::Event, _>| {
        let Ok(event) = res else { return };
        if !event.kind.is_modify() && !event.kind.is_create() {
            return;
        }
        let relevant =
            event.paths.iter().any(|path| is_relevant_config_event_path(&watched_config_dir, path));
        if relevant {
            on_change();
        }
    })
    .ok()?;

    watcher.watch(&config_path, RecursiveMode::NonRecursive).ok()?;

    tracing::info!(?config_path, "config file watcher started");
    Some(watcher)
}

/// The live surfaces that a config reload must reapply.
///
/// Mirrors the legacy `ConfigReloadPlan`, narrowed to the surfaces the GPUI
/// spike consumes directly: the resolved terminal/chrome theme, the font
/// metrics that drive cell-grid layout, and the root-background opacity. Any
/// keybinding edit is always reapplied (the [`Bindings`] are re-parsed
/// unconditionally), so it needs no flag here.
///
/// The three flags are stored as a small bitfield rather than three `bool`
/// fields to satisfy the workspace `clippy::struct_excessive_bools` gate
/// (`max-struct-bools = 2`), mirroring [`crate::input::KittyFlags`]; the public
/// surface is the accessor methods below.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ConfigReloadPlan {
    bits: u8,
}

impl ConfigReloadPlan {
    /// The resolved terminal/chrome theme changed.
    const THEME_CHANGED: u8 = 1 << 0;
    /// A font metric that drives the cell grid changed.
    const FONT_CHANGED: u8 = 1 << 1;
    /// The root-background opacity changed.
    const OPACITY_CHANGED: u8 = 1 << 2;

    fn analyze(old: &ScribeConfig, new: &ScribeConfig) -> Self {
        let mut bits = 0;
        if theme_reload_needed(old, new) {
            bits |= Self::THEME_CHANGED;
        }
        if font_params_changed(old, new) {
            bits |= Self::FONT_CHANGED;
        }
        if (old.appearance.opacity - new.appearance.opacity).abs() > f32::EPSILON {
            bits |= Self::OPACITY_CHANGED;
        }
        Self { bits }
    }

    /// `true` when the resolved theme (preset name, inline `[theme]`, or
    /// external file selection) changed and must be reapplied.
    #[must_use]
    pub const fn theme_changed(self) -> bool {
        self.bits & Self::THEME_CHANGED != 0
    }

    /// `true` when a font metric that drives the cell grid changed and a
    /// layout resize must follow.
    #[must_use]
    pub const fn font_changed(self) -> bool {
        self.bits & Self::FONT_CHANGED != 0
    }

    /// `true` when the root-background opacity changed.
    #[must_use]
    pub const fn opacity_changed(self) -> bool {
        self.bits & Self::OPACITY_CHANGED != 0
    }

    /// `true` when any live surface changed and a repaint is warranted.
    #[must_use]
    pub const fn any_changed(self) -> bool {
        self.bits != 0
    }
}

/// Whether the resolved theme must be recomputed.
///
/// Matches the legacy heuristic: a changed preset name, a changed inline
/// `[theme]` section, or an external theme file selection (which may have
/// changed on disk) all force a re-resolve.
fn theme_reload_needed(old: &ScribeConfig, new: &ScribeConfig) -> bool {
    let theme_name_changed = old.appearance.theme != new.appearance.theme;
    let inline_theme_changed = old.theme != new.theme;
    let external_theme_selected = new.appearance.theme != "custom"
        && scribe_common::theme::resolve_preset(&new.appearance.theme).is_none();
    theme_name_changed || inline_theme_changed || external_theme_selected
}

/// Whether any font metric that affects the cell grid changed.
fn font_params_changed(old: &ScribeConfig, new: &ScribeConfig) -> bool {
    old.appearance.font != new.appearance.font
        || (old.appearance.font_size - new.appearance.font_size).abs() > f32::EPSILON
        || old.appearance.font_weight != new.appearance.font_weight
        || old.appearance.font_weight_bold != new.appearance.font_weight_bold
        || old.appearance.ligatures != new.appearance.ligatures
        || old.appearance.line_padding != new.appearance.line_padding
}

/// A resolved snapshot of everything the client derives from config.
///
/// Bundles the raw [`ScribeConfig`], the resolved terminal [`Theme`] and its
/// derived [`ChromeColors`], and the parsed [`Bindings`]. Constructing or
/// reloading it runs the same `scribe-common` resolution the legacy client
/// used, so removed appearance keys deserialize inertly and inline `[theme]`
/// sections resolve identically.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// The raw parsed config.
    pub config: ScribeConfig,
    /// The resolved terminal + chrome theme.
    pub theme: Theme,
    /// Chrome colors derived from the theme (a copy of `theme.chrome`).
    pub chrome: ChromeColors,
    /// Parsed keybindings.
    pub bindings: Bindings,
}

impl ClientConfig {
    /// Build a snapshot from an already-parsed [`ScribeConfig`].
    #[must_use]
    pub fn from_config(config: ScribeConfig) -> Self {
        let theme = resolve_theme(&config);
        let chrome = theme.chrome;
        let bindings = Bindings::parse(&config.keybindings);
        Self { config, theme, chrome, bindings }
    }

    /// Load the active config from disk and resolve it into a snapshot.
    ///
    /// Falls back to [`ScribeConfig::default`] if loading fails, matching the
    /// legacy startup path (a broken config never blocks the client).
    #[must_use]
    pub fn load() -> Self {
        let config = load_config().unwrap_or_else(|error| {
            tracing::warn!("config load failed, using defaults: {error}");
            ScribeConfig::default()
        });
        Self::from_config(config)
    }

    /// Replace this snapshot with a freshly parsed config, returning the plan of
    /// live surfaces that changed.
    ///
    /// The theme, chrome colors, and keybindings are always recomputed so a
    /// saved edit reapplies without a restart; the returned [`ConfigReloadPlan`]
    /// tells the caller which surfaces (theme, font, opacity) actually differ so
    /// it can skip redundant reapply work.
    pub fn reload(&mut self, new_config: ScribeConfig) -> ConfigReloadPlan {
        let plan = ConfigReloadPlan::analyze(&self.config, &new_config);
        *self = Self::from_config(new_config);
        plan
    }

    /// Reload from disk, returning the change plan.
    #[must_use]
    pub fn reload_from_disk(&mut self) -> ConfigReloadPlan {
        let new_config = load_config().unwrap_or_else(|error| {
            tracing::warn!("config reload failed, keeping current config: {error}");
            self.config.clone()
        });
        self.reload(new_config)
    }
}

/// A cross-thread "the config file changed" flag.
///
/// The `notify` watcher callback runs on its own thread and must not touch GPUI
/// state, so it only calls [`ConfigChangeSignal::signal`], which bumps an atomic
/// generation. The GPUI foreground polls with [`ConfigChangeSignal::take_change`],
/// which collapses any number of events fired since the last poll into a single
/// reload — editors that save by delete-and-recreate emit several events per
/// save, and the reload is idempotent, so collapsing them is both correct and
/// cheaper than reloading once per event.
#[derive(Clone, Debug, Default)]
pub struct ConfigChangeSignal {
    generation: Arc<AtomicU64>,
}

impl ConfigChangeSignal {
    /// A fresh signal with no pending change.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that the on-disk config changed. Callable from any thread.
    pub fn signal(&self) {
        self.generation.fetch_add(1, Ordering::Release);
    }

    /// The current change generation.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Consume any change recorded since `seen`, advancing `seen` to the
    /// current generation. Returns `true` when a reload is due.
    pub fn take_change(&self, seen: &mut u64) -> bool {
        let current = self.generation();
        if current == *seen {
            return false;
        }
        *seen = current;
        true
    }
}

/// The terminal window's live config: the resolved snapshot, the file watcher
/// keeping it fresh, and the change signal the GPUI foreground polls.
///
/// Dropping this stops the watcher, so the window must hold it for its whole
/// lifetime. [`ConfigRuntime::poll_reload`] is the single entry point the UI
/// thread calls: it returns `None` when nothing changed and otherwise reloads
/// from disk and hands back the [`ConfigReloadPlan`] to reapply.
pub struct ConfigRuntime {
    config: ClientConfig,
    signal: ConfigChangeSignal,
    /// Kept alive for its side effect: dropping it stops the file watcher.
    _watcher: Option<RecommendedWatcher>,
    seen: u64,
}

impl ConfigRuntime {
    /// Load the active config and start watching its directory for edits.
    ///
    /// A watcher that fails to start (missing config dir, inotify exhaustion)
    /// is logged and left as `None`: the window still runs, it just will not
    /// live-reload, exactly as the legacy client behaves.
    #[must_use]
    pub fn start() -> Self {
        let signal = ConfigChangeSignal::new();
        let watcher_signal = signal.clone();
        let watcher = start_config_watcher(move || watcher_signal.signal());
        if watcher.is_none() {
            tracing::warn!("config file watcher unavailable; live reload disabled");
        }
        Self { config: ClientConfig::load(), signal, _watcher: watcher, seen: 0 }
    }

    /// Build a runtime around an already-resolved snapshot with no watcher.
    ///
    /// Used by headless tests (and any caller driving reloads itself) so the
    /// full poll/reload/apply path can run without touching the real config
    /// directory.
    #[must_use]
    pub fn detached(config: ClientConfig) -> Self {
        Self { config, signal: ConfigChangeSignal::new(), _watcher: None, seen: 0 }
    }

    /// The signal the watcher bumps; clone it to drive reloads by hand.
    #[must_use]
    pub fn signal(&self) -> ConfigChangeSignal {
        self.signal.clone()
    }

    /// The current resolved snapshot (config, theme, chrome, bindings).
    #[must_use]
    pub const fn config(&self) -> &ClientConfig {
        &self.config
    }

    /// The parsed keybindings currently in force.
    ///
    /// [`ClientConfig::reload`] re-parses these on every reload, so a saved
    /// keybinding edit is live the moment the caller reads them again.
    #[must_use]
    pub const fn bindings(&self) -> &Bindings {
        &self.config.bindings
    }

    /// The root-background opacity from the live config, exactly as written.
    ///
    /// This is the delivery point for the reload plan's `opacity_changed()`
    /// signal: the window's opacity hook reads it whenever the plan flags a
    /// change. The value is returned unclamped — the config file is not
    /// validated on load — so every consumer runs it through
    /// [`clamp_opacity`](crate::opacity::clamp_opacity) before painting with it.
    #[must_use]
    pub const fn opacity(&self) -> f32 {
        self.config.config.appearance.opacity
    }

    /// Consume a pending change without reloading. Returns `true` when the
    /// watcher signalled since the last poll.
    pub fn take_pending(&mut self) -> bool {
        self.signal.take_change(&mut self.seen)
    }

    /// Poll the watcher and, when a change is pending, reload from disk.
    ///
    /// Returns the [`ConfigReloadPlan`] for the reload, or `None` when nothing
    /// changed. A failed parse keeps the current config (see
    /// [`ClientConfig::reload_from_disk`]) and reports an empty plan, so a
    /// half-written file mid-save never blanks the window.
    pub fn poll_reload(&mut self) -> Option<ConfigReloadPlan> {
        self.take_pending().then(|| self.config.reload_from_disk())
    }

    /// Apply an explicit config as if it had just been read from disk.
    ///
    /// The test seam behind [`Self::poll_reload`]: it runs the same
    /// [`ClientConfig::reload`] bookkeeping without any filesystem access.
    pub fn reload(&mut self, new_config: ScribeConfig) -> ConfigReloadPlan {
        self.config.reload(new_config)
    }
}

#[cfg(test)]
mod tests;

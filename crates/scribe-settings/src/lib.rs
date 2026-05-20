pub mod apply;
pub mod server_action;
pub mod singleton;
pub mod state;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rust_embed::Embed;
use scribe_common::{
    protocol::{PreflightError, ReleaseListResultState, UpdateCheckResultState},
    settings_window::{SettingsWindowAnchor, centered_settings_position},
};

/// Maximum time to wait for a manual update-check response from the server.
/// The server-side check itself can take ~10 s including the 5 s retry, so we
/// add headroom for a slow network or a busy updater task. After this elapses
/// the user sees a "Check failed: …" message and the worker thread aborts.
const UPDATE_CHECK_TIMEOUT: Duration = Duration::from_secs(30);
/// Write-only timeout for the fire-and-forget `TriggerUpdate` IPC. There is no
/// reply, so the value bounds only the `connect` + `write_all` calls in
/// [`server_action::request_trigger_update`]; matched to the check path so
/// the two transient IPCs share latency expectations.
const TRIGGER_UPDATE_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum time to wait for a `ListReleases` response from the server.
///
/// SC-001 / SC-003 in the feature spec target a 5 s success window on a
/// typical broadband connection. The 7 s budget here is the upper bound for
/// failure detection: it gives a slow-but-eventually-successful fetch enough
/// headroom to land while still flipping to `Failed` for genuinely stuck
/// requests.
const RELEASE_LIST_TIMEOUT: Duration = Duration::from_secs(7);

/// Maximum time to wait for an `EnvPreflight` response from the server.
///
/// Matches [`RELEASE_LIST_TIMEOUT`]. The preflight is a low-cost keystore
/// probe but the OS may need a one-time unlock prompt on first access (macOS
/// Keychain, GNOME Keyring / `KWallet` on Linux), so we give the user some
/// room before timing out and folding the result into
/// `EnvPreflightOutcome::Err(PreflightError::Unknown(_))`.
const ENV_PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(7);

/// Embedded web assets (HTML, CSS, JS) for the settings UI.
#[derive(Embed)]
#[folder = "src/assets/"]
struct Assets;

/// Saved settings window geometry, returned on close.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct SettingsWindowGeometry {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// Build a self-contained HTML document by inlining the CSS and JS assets.
///
/// The resulting HTML can be loaded directly via `wry::WebViewBuilder::with_html`.
pub fn build_html() -> Result<String, String> {
    let html_bytes = Assets::get("settings.html")
        .ok_or_else(|| String::from("embedded asset settings.html not found"))?;
    let css_bytes = Assets::get("settings.css")
        .ok_or_else(|| String::from("embedded asset settings.css not found"))?;
    let js_bytes = Assets::get("settings.js")
        .ok_or_else(|| String::from("embedded asset settings.js not found"))?;

    let html = std::str::from_utf8(&html_bytes.data)
        .map_err(|e| format!("settings.html is not valid UTF-8: {e}"))?;
    let css = std::str::from_utf8(&css_bytes.data)
        .map_err(|e| format!("settings.css is not valid UTF-8: {e}"))?;
    let js = std::str::from_utf8(&js_bytes.data)
        .map_err(|e| format!("settings.js is not valid UTF-8: {e}"))?;

    // Replace the external CSS link with an inline <style> block.
    let html = html.replace(
        r#"<link rel="stylesheet" href="settings.css">"#,
        &format!("<style>\n{css}\n</style>"),
    );

    // Replace the external JS script tag with an inline <script> block.
    let html = html
        .replace(r#"<script src="settings.js"></script>"#, &format!("<script>\n{js}\n</script>"));

    Ok(html)
}

/// Query the system for installed monospace font families.
///
/// Returns a sorted, deduplicated list of font family names.
fn list_monospace_fonts() -> Vec<String> {
    use std::collections::BTreeSet;

    let mut db = fontdb::Database::new();
    db.load_system_fonts();

    let mut families = BTreeSet::new();
    for info in db.faces() {
        if info.monospaced {
            for (name, _) in &info.families {
                families.insert(name.clone());
            }
        }
    }

    families.into_iter().collect()
}

/// Inject keybinding defaults into the webview for reset-to-default support.
fn inject_keybinding_defaults(webview: &wry::WebView) {
    let defaults = scribe_common::config::KeybindingsConfig::default();
    let json = serde_json::to_string(&defaults).unwrap_or_else(|_| String::from("{}"));
    let script = format!(
        "if (typeof loadKeybindingDefaults === 'function') {{ loadKeybindingDefaults({json}); }}"
    );
    if let Err(e) = webview.evaluate_script(&script) {
        tracing::warn!("failed to inject keybinding defaults into settings webview: {e}");
    }
}

/// Inject all theme preset colors into the webview.
fn inject_theme_colors(webview: &wry::WebView) {
    use serde_json::{Map, Value};

    let mut map = Map::new();
    for name in scribe_common::theme::all_preset_names() {
        let Some(theme) = scribe_common::theme::resolve_preset(name) else {
            continue;
        };
        let key = name.replace('-', "_");
        let ansi: Vec<Value> = theme
            .ansi_colors
            .iter()
            .map(|c| Value::String(scribe_common::theme::rgba_to_hex(*c)))
            .collect();
        let mut entry = Map::new();
        entry.insert(String::from("name"), Value::String(theme.name.into_owned()));
        entry.insert(
            String::from("fg"),
            Value::String(scribe_common::theme::rgba_to_hex(theme.foreground)),
        );
        entry.insert(
            String::from("bg"),
            Value::String(scribe_common::theme::rgba_to_hex(theme.background)),
        );
        entry.insert(
            String::from("cursor"),
            Value::String(scribe_common::theme::rgba_to_hex(theme.cursor)),
        );
        entry.insert(
            String::from("cursor_accent"),
            Value::String(scribe_common::theme::rgba_to_hex(theme.cursor_accent)),
        );
        entry.insert(
            String::from("selection"),
            Value::String(scribe_common::theme::rgba_to_hex(theme.selection)),
        );
        entry.insert(
            String::from("selection_fg"),
            Value::String(scribe_common::theme::rgba_to_hex(theme.selection_foreground)),
        );
        entry.insert(String::from("ansi"), Value::Array(ansi));
        map.insert(key, Value::Object(entry));
    }
    let json = serde_json::to_string(&map).unwrap_or_else(|_| String::from("{}"));
    let script =
        format!("if (typeof loadThemeColors === 'function') {{ loadThemeColors({json}); }}");
    if let Err(e) = webview.evaluate_script(&script) {
        tracing::warn!("failed to inject theme colors into settings webview: {e}");
    }
}

/// Inject the available font list into the webview.
fn inject_font_list(webview: &wry::WebView) {
    let fonts = list_monospace_fonts();
    let fonts_json = serde_json::to_string(&fonts).unwrap_or_else(|_| String::from("[]"));
    let script =
        format!("if (typeof loadFontList === 'function') {{ loadFontList({fonts_json}); }}");
    if let Err(e) = webview.evaluate_script(&script) {
        tracing::warn!("failed to inject font list into settings webview: {e}");
    }
}

/// Inject `window.SCRIBE_PLATFORM` into the webview so JS can adapt to the host OS.
fn inject_platform(webview: &wry::WebView) {
    let platform = current_platform_string();
    let script = format!(
        "if (typeof setPlatform === 'function') {{ setPlatform(\"{platform}\"); }} else {{ window.SCRIBE_PLATFORM = \"{platform}\"; }}"
    );
    if let Err(e) = webview.evaluate_script(&script) {
        tracing::warn!("failed to inject platform into settings webview: {e}");
    }
}

/// Compile-time platform string used by both the legacy `window.SCRIBE_PLATFORM`
/// path (`inject_platform`) and the new `window.SCRIBE_BOOTSTRAP.platform` field
/// passed via the pre-page-load script. Keeping a single function ensures the
/// two channels never disagree.
fn current_platform_string() -> &'static str {
    if cfg!(target_os = "macos") { "macos" } else { "linux" }
}

/// Build the JS snippet that defines `window.SCRIBE_BOOTSTRAP` before the
/// settings page loads. The values are JSON-escaped via `serde_json::to_string`
/// so an embedded `"` (or any other JS-special character) in a future version
/// or platform string cannot break the literal or open an injection vector.
///
/// Output looks like:
/// `window.SCRIBE_BOOTSTRAP = { version: "0.0.0-dev", platform: "linux" };`
fn bootstrap_script(version: &str, platform: &str) -> String {
    // `serde_json::to_string` always succeeds for `&str` (no IO, no recursion).
    let version_lit = serde_json::to_string(version).unwrap_or_else(|_| String::from("\"\""));
    let platform_lit = serde_json::to_string(platform).unwrap_or_else(|_| String::from("\"\""));
    format!("window.SCRIBE_BOOTSTRAP = {{ version: {version_lit}, platform: {platform_lit} }};")
}

/// Load the current config and serialise it for webview injection.
fn load_config_json() -> String {
    let config = scribe_common::config::load_config().unwrap_or_else(|e| {
        tracing::warn!("failed to load config: {e}, using defaults");
        scribe_common::config::ScribeConfig::default()
    });
    serde_json::to_string(&config).unwrap_or_else(|e| {
        tracing::warn!("failed to serialize config: {e}");
        String::from("{}")
    })
}

/// Inject the initial runtime state needed by the settings frontend.
fn inject_initial_webview_state(webview: &wry::WebView, config_json: &str) {
    inject_platform(webview);

    let init_script =
        format!("if (typeof loadConfig === 'function') {{ loadConfig({config_json}); }}");
    if let Err(e) = webview.evaluate_script(&init_script) {
        tracing::warn!("failed to inject config into settings webview: {e}");
    }

    inject_keybinding_defaults(webview);
    inject_theme_colors(webview);
    inject_font_list(webview);
}

/// Extract the `type` field from a webview IPC request body.
fn settings_ipc_request_type(body: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(str::to_owned))
}

/// Push a manual update-check result into the webview's `updateCheckResult`
/// callback. Always called on the UI thread (GTK main loop on Linux, the tao
/// event loop on macOS) so `evaluate_script` is safe.
fn inject_update_check_result(
    webview_ref: &std::rc::Rc<std::cell::RefCell<Option<wry::WebView>>>,
    state: &UpdateCheckResultState,
) {
    let json = match serde_json::to_string(state) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("failed to serialise UpdateCheckResultState: {e}");
            return;
        }
    };
    let script =
        format!("if (typeof updateCheckResult === 'function') {{ updateCheckResult({json}); }}");
    if let Some(wv) = webview_ref.borrow().as_ref() {
        if let Err(e) = wv.evaluate_script(&script) {
            tracing::warn!("failed to inject update check result: {e}");
        }
    }
}

/// Whether `url` is safe to hand to the platform browser opener.
///
/// The webview can ask the host to open arbitrary URLs via the
/// `open_external_url` IPC message. To prevent the renderer from coercing the
/// host into invoking `xdg-open`/`open` on `javascript:`, `file:`, `data:`,
/// `vbscript:`, etc., we accept only `http://` and `https://` (case-insensitive)
/// and drop everything else.
fn external_url_is_safe(url: &str) -> bool {
    let lower = url.to_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

/// Hand off `url` to the platform browser opener iff [`external_url_is_safe`]
/// accepts it. Non-http(s) schemes are dropped with a `tracing::warn!` log,
/// not silently ignored, so security audits can spot misbehaving renderers.
fn dispatch_open_external_url(url: &str) {
    if !external_url_is_safe(url) {
        tracing::warn!("rejected non-http(s) external URL: {url}");
        return;
    }
    let opener = if cfg!(target_os = "linux") { "xdg-open" } else { "open" };
    match std::process::Command::new(opener).arg(url).spawn() {
        Ok(child) => drop(child),
        Err(e) => tracing::warn!("failed to launch {opener} for external URL {url}: {e}"),
    }
}

/// Format a `SystemTime` as an ISO-8601 / RFC 3339 UTC timestamp, e.g.
/// `2026-05-10T13:45:09Z`.
///
/// This is the `fetched_at` value the host reports to the webview alongside
/// fresh and stale release lists. We avoid pulling `chrono` / `time` into
/// `scribe-settings` for a single timestamp by deriving the date components
/// directly from the seconds since the Unix epoch using the civil-from-days
/// algorithm Howard Hinnant published; pre-1970 timestamps clamp to the
/// epoch, which is fine because this is only ever called with `SystemTime::now()`.
fn format_iso_utc(time: SystemTime) -> String {
    let secs = time
        .duration_since(UNIX_EPOCH)
        .map_or(0_i64, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
    let (year, month, day, hour, minute, second) = unix_seconds_to_utc(secs);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Decompose an integer count of UTC seconds since the Unix epoch into
/// `(year, month, day, hour, minute, second)` using Howard Hinnant's
/// `civil_from_days` algorithm. Negative values clamp to the epoch.
fn unix_seconds_to_utc(seconds: i64) -> (i32, u32, u32, u32, u32, u32) {
    let seconds = seconds.max(0);
    // 86_400 seconds per day; 0..86_399 inclusive can never overflow `u32`.
    let days = seconds / 86_400;
    let day_seconds = seconds.rem_euclid(86_400);
    let hour = u32::try_from(day_seconds / 3_600).unwrap_or(0);
    let minute = u32::try_from((day_seconds / 60) % 60).unwrap_or(0);
    let second = u32::try_from(day_seconds % 60).unwrap_or(0);

    // civil_from_days: shift epoch from 1970-01-01 to 0000-03-01.
    let z = days + 719_468;
    let era = if z >= 0 { z / 146_097 } else { (z - 146_096) / 146_097 };
    let doe = z - era * 146_097; // day of era, in [0, 146_096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // year of era
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // day of year (March-based)
    let mp = (5 * doy + 2) / 153;
    let day = u32::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1);
    let month = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1);
    let year = i32::try_from(if month <= 2 { y + 1 } else { y }).unwrap_or(1970);
    (year, month, day, hour, minute, second)
}

/// Build the JSON payload `window.SCRIBE_ON_RELEASE_LIST` expects for `state`.
///
/// Per `contracts/releases-protocol.md` §2.2 the host transforms the
/// externally-tagged `ReleaseListResultState` into a flat object with a
/// lower-case `state` discriminator and conditional `releases`, `reason`, and
/// `fetched_at` siblings. Building the JSON manually keeps the host in full
/// control of which fields are present per state and lets `serde_json` handle
/// HTML-/JS-safe escaping of the embedded strings.
fn release_list_payload_json(
    state: &ReleaseListResultState,
    fetched_at: SystemTime,
) -> Result<String, serde_json::Error> {
    use serde_json::{Map, Value, json};

    let fetched_at_iso = format_iso_utc(fetched_at);
    let mut map: Map<String, Value> = Map::new();
    match state {
        ReleaseListResultState::Fresh { releases } => {
            map.insert("state".to_owned(), Value::String("fresh".to_owned()));
            map.insert("releases".to_owned(), serde_json::to_value(releases)?);
            map.insert("fetched_at".to_owned(), Value::String(fetched_at_iso));
        }
        ReleaseListResultState::Stale { releases, reason } => {
            map.insert("state".to_owned(), Value::String("stale".to_owned()));
            map.insert("releases".to_owned(), serde_json::to_value(releases)?);
            map.insert("reason".to_owned(), Value::String(reason.clone()));
            map.insert("fetched_at".to_owned(), Value::String(fetched_at_iso));
        }
        ReleaseListResultState::Failed { reason } => {
            map.insert("state".to_owned(), Value::String("failed".to_owned()));
            map.insert("reason".to_owned(), Value::String(reason.clone()));
        }
    }
    serde_json::to_string(&Value::Object(map)).or_else(|e| {
        // Final defensive path: a serde-named `json!` literal can never fail
        // serialise but the compiler does not know that here.
        let fallback = json!({ "state": "failed", "reason": format!("payload error: {e}") });
        serde_json::to_string(&fallback)
    })
}

/// Push a manual release-list result into the webview's
/// `window.SCRIBE_ON_RELEASE_LIST` callback. Always called on the UI thread
/// (GTK main loop on Linux, the tao event loop on macOS) so `evaluate_script`
/// is safe.
fn inject_release_list_result(
    webview_ref: &std::rc::Rc<std::cell::RefCell<Option<wry::WebView>>>,
    state: &ReleaseListResultState,
) {
    let payload = match release_list_payload_json(state, SystemTime::now()) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("failed to serialise ReleaseListResultState: {e}");
            return;
        }
    };
    let script = format!(
        "if (typeof window.SCRIBE_ON_RELEASE_LIST === 'function') {{ window.SCRIBE_ON_RELEASE_LIST({payload}); }}"
    );
    if let Some(wv) = webview_ref.borrow().as_ref() {
        if let Err(e) = wv.evaluate_script(&script) {
            tracing::warn!("failed to inject release list result: {e}");
        }
    }
}

/// Build the JSON payload `window.SCRIBE_ON_ENV_PREFLIGHT_RESULT` expects.
///
/// Per `contracts/env-preflight.md` the host sends a flat object with a
/// boolean `ok` discriminator and a conditional `error` sibling whose
/// `type` matches the snake-case variant name of [`PreflightError`].
///
/// Built manually (not via `serde`) because `PreflightError::Unknown(String)`
/// is a tuple variant with internal `#[serde(tag = "type")]` tagging that
/// `serde_json` refuses to serialize; the on-wire msgpack codec
/// (`rmp_serde`) handles the tuple-variant fine, but the webview-bound JSON
/// needs the flat `{type, reason?}` shape regardless of how serde would
/// render the same enum.
fn env_preflight_payload_json(outcome: &server_action::EnvPreflightOutcome) -> Option<String> {
    use server_action::EnvPreflightOutcome;
    let value = match outcome {
        EnvPreflightOutcome::Ok => serde_json::json!({"ok": true}),
        EnvPreflightOutcome::Err(e) => serde_json::json!({
            "ok": false,
            "error": preflight_error_json(e),
        }),
    };
    serde_json::to_string(&value).ok()
}

/// Render a single [`PreflightError`] as the inner `error` object the
/// webview's `envPreflightErrorMessage` switch expects. Carried out manually
/// for the same reason as [`env_preflight_payload_json`].
fn preflight_error_json(e: &PreflightError) -> serde_json::Value {
    match e {
        PreflightError::KeychainLocked => serde_json::json!({"type": "keychain_locked"}),
        PreflightError::SecretServiceUnavailable => {
            serde_json::json!({"type": "secret_service_unavailable"})
        }
        PreflightError::KeystoreAccessDenied => {
            serde_json::json!({"type": "keystore_access_denied"})
        }
        PreflightError::Unknown(reason) => {
            serde_json::json!({"type": "unknown", "reason": reason})
        }
    }
}

/// Push an env-preflight outcome into the webview's
/// `window.SCRIBE_ON_ENV_PREFLIGHT_RESULT` callback. Always called on the UI
/// thread (GTK main loop on Linux, the tao event loop on macOS) so
/// `evaluate_script` is safe.
fn inject_env_preflight_result(
    webview_ref: &std::rc::Rc<std::cell::RefCell<Option<wry::WebView>>>,
    outcome: &server_action::EnvPreflightOutcome,
) {
    let Some(payload) = env_preflight_payload_json(outcome) else {
        tracing::warn!("failed to serialise EnvPreflightOutcome");
        return;
    };
    let script = format!(
        "if (typeof window.SCRIBE_ON_ENV_PREFLIGHT_RESULT === 'function') {{ window.SCRIBE_ON_ENV_PREFLIGHT_RESULT({payload}); }}"
    );
    if let Some(wv) = webview_ref.borrow().as_ref() {
        if let Err(e) = wv.evaluate_script(&script) {
            tracing::warn!("failed to inject env preflight result: {e}");
        }
    }
}

/// Push a native workspace-root picker result into the webview callback.
/// Always called on the UI thread for the active platform backend.
fn inject_workspace_root_choice(
    webview_ref: &std::rc::Rc<std::cell::RefCell<Option<wry::WebView>>>,
    path: &str,
) {
    let payload = serde_json::json!({ "path": path });
    let script = format!(
        "if (typeof window.SCRIBE_ON_WORKSPACE_ROOT_CHOSEN === 'function') {{ window.SCRIBE_ON_WORKSPACE_ROOT_CHOSEN({payload}); }}"
    );
    if let Some(wv) = webview_ref.borrow().as_ref() {
        if let Err(e) = wv.evaluate_script(&script) {
            tracing::warn!("failed to inject workspace root choice: {e}");
        }
    }
}

/// Shared cell holding the currently-active glib timeout source for an
/// in-flight manual update check on Linux. Storing the [`SourceId`] lets the
/// window-close path explicitly remove the source before the webview is
/// dropped, avoiding a renderer-cleanup crash on some `WebKitGTK` versions
/// when the closure outlives the underlying widget.
#[cfg(target_os = "linux")]
type ActiveCheckSource = std::rc::Rc<std::cell::RefCell<Option<gtk::glib::SourceId>>>;

#[cfg(target_os = "linux")]
fn new_active_check_source() -> ActiveCheckSource {
    std::rc::Rc::new(std::cell::RefCell::new(None))
}

/// Linux: spawn a worker thread to talk to the server, then drain the result
/// back to the webview via a `glib` timeout source on the GTK main loop.
///
/// We poll a `std::sync::mpsc` channel every 100 ms because `glib::idle_add`
/// requires `Send` closures and our `Rc<RefCell<…>>` webview reference is not
/// `Send`. Polling at this cadence is cheap and the latency between worker
/// completion and UI update is imperceptible.
///
/// The registered [`SourceId`] is recorded in `active_source` so the window
/// shutdown path can cancel it explicitly. A second click while a check is
/// already in flight cancels the prior poll and starts fresh.
#[cfg(target_os = "linux")]
fn dispatch_check_for_updates_linux(
    webview_ref: &std::rc::Rc<std::cell::RefCell<Option<wry::WebView>>>,
    active_source: &ActiveCheckSource,
) {
    if let Some(prev) = active_source.borrow_mut().take() {
        prev.remove();
    }

    let webview_clone = std::rc::Rc::clone(webview_ref);
    let active_for_closure = std::rc::Rc::clone(active_source);
    let (tx, rx) = std::sync::mpsc::channel::<UpdateCheckResultState>();

    std::thread::spawn(move || {
        let state = server_action::request_update_check(UPDATE_CHECK_TIMEOUT);
        drop(tx.send(state));
    });

    let source_id =
        gtk::glib::timeout_add_local(Duration::from_millis(100), move || match rx.try_recv() {
            Ok(state) => {
                inject_update_check_result(&webview_clone, &state);
                *active_for_closure.borrow_mut() = None;
                gtk::glib::ControlFlow::Break
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => gtk::glib::ControlFlow::Continue,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                *active_for_closure.borrow_mut() = None;
                gtk::glib::ControlFlow::Break
            }
        });
    *active_source.borrow_mut() = Some(source_id);
}

/// Cancel any in-flight manual-check polling source. Called from the window
/// shutdown path so the closure (which holds an `Rc` clone of the webview)
/// drops before the underlying widget is torn down.
#[cfg(target_os = "linux")]
fn cancel_active_check_source(active_source: &ActiveCheckSource) {
    if let Some(source) = active_source.borrow_mut().take() {
        source.remove();
    }
}

/// Spawn a worker thread that asks the server to start an update install.
///
/// Fire-and-forget: the server has no reply for `TriggerUpdate`, and install
/// progress is broadcast only to registered clients (which the settings
/// window is not). The in-client overlay still owns the user-facing progress
/// and restart-required prompt; the settings UI flips its button to
/// "Installing…" the moment the request is dispatched and stays there until
/// the user re-checks.
///
/// Intentionally not `#[cfg]`-guarded per platform — unlike the check and
/// release-list dispatchers, this function has no reply to route back onto a
/// platform-specific event loop, so the same `std::thread::spawn` body works
/// on both Linux (GTK) and macOS (tao).
fn dispatch_trigger_update() {
    std::thread::spawn(|| {
        if let Err(reason) = server_action::request_trigger_update(TRIGGER_UPDATE_TIMEOUT) {
            tracing::warn!("trigger update transport error: {reason}");
        }
    });
}

/// Linux: spawn a worker thread to issue a `ListReleases` request to the
/// server, then drain the result back to the webview via a `glib` timeout
/// source on the GTK main loop.
///
/// Mirrors [`dispatch_check_for_updates_linux`]: the worker thread cannot
/// touch the webview directly (the `Rc<RefCell<…>>` reference is `!Send`), so
/// we hand the result back over a `mpsc` channel that a 100 ms `glib` timeout
/// source polls on the UI thread. A second click while a request is already
/// in flight cancels the prior poll and starts fresh, just like the update
/// check path.
#[cfg(target_os = "linux")]
fn dispatch_release_list_request_linux(
    webview_ref: &std::rc::Rc<std::cell::RefCell<Option<wry::WebView>>>,
    active_source: &ActiveCheckSource,
) {
    if let Some(prev) = active_source.borrow_mut().take() {
        prev.remove();
    }

    let webview_clone = std::rc::Rc::clone(webview_ref);
    let active_for_closure = std::rc::Rc::clone(active_source);
    let (tx, rx) = std::sync::mpsc::channel::<ReleaseListResultState>();

    std::thread::spawn(move || {
        let state = server_action::request_release_list(RELEASE_LIST_TIMEOUT);
        drop(tx.send(state));
    });

    let source_id =
        gtk::glib::timeout_add_local(Duration::from_millis(100), move || match rx.try_recv() {
            Ok(state) => {
                inject_release_list_result(&webview_clone, &state);
                *active_for_closure.borrow_mut() = None;
                gtk::glib::ControlFlow::Break
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => gtk::glib::ControlFlow::Continue,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                *active_for_closure.borrow_mut() = None;
                gtk::glib::ControlFlow::Break
            }
        });
    *active_source.borrow_mut() = Some(source_id);
}

/// Linux: spawn a worker thread to issue an `EnvPreflight` request to the
/// server, then drain the result back to the webview via a `glib` timeout
/// source on the GTK main loop.
///
/// Mirrors [`dispatch_release_list_request_linux`]: the worker thread cannot
/// touch the webview directly (the `Rc<RefCell<…>>` reference is `!Send`), so
/// we hand the outcome back over a `mpsc` channel that a 100 ms `glib`
/// timeout source polls on the UI thread. A second click while a request is
/// already in flight cancels the prior poll and starts fresh, just like the
/// update-check and release-list paths.
#[cfg(target_os = "linux")]
fn dispatch_env_preflight_request_linux(
    webview_ref: &std::rc::Rc<std::cell::RefCell<Option<wry::WebView>>>,
    active_source: &ActiveCheckSource,
) {
    if let Some(prev) = active_source.borrow_mut().take() {
        prev.remove();
    }

    let webview_clone = std::rc::Rc::clone(webview_ref);
    let active_for_closure = std::rc::Rc::clone(active_source);
    let (tx, rx) = std::sync::mpsc::channel::<server_action::EnvPreflightOutcome>();

    std::thread::spawn(move || {
        let outcome = server_action::request_env_preflight(ENV_PREFLIGHT_TIMEOUT);
        drop(tx.send(outcome));
    });

    let source_id =
        gtk::glib::timeout_add_local(Duration::from_millis(100), move || match rx.try_recv() {
            Ok(outcome) => {
                inject_env_preflight_result(&webview_clone, &outcome);
                *active_for_closure.borrow_mut() = None;
                gtk::glib::ControlFlow::Break
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => gtk::glib::ControlFlow::Continue,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                *active_for_closure.borrow_mut() = None;
                gtk::glib::ControlFlow::Break
            }
        });
    *active_source.borrow_mut() = Some(source_id);
}

/// macOS: spawn a worker thread to talk to the server, then deliver the result
/// as a tao user event so the main event loop can call `evaluate_script` on
/// the UI thread. Mirrors how the existing `FocusWindow` / `QuitWindow` events
/// hand off from the singleton-listener thread to the event loop.
#[cfg(not(target_os = "linux"))]
fn dispatch_check_for_updates_macos(proxy: &tao::event_loop::EventLoopProxy<TaoUserEvent>) {
    let proxy = proxy.clone();
    std::thread::spawn(move || {
        let state = server_action::request_update_check(UPDATE_CHECK_TIMEOUT);
        drop(proxy.send_event(TaoUserEvent::UpdateCheckResult(state)));
    });
}

/// macOS: spawn a worker thread to issue a `ListReleases` request to the
/// server, then deliver the result as a tao user event so the main event loop
/// can call `evaluate_script` on the UI thread.
#[cfg(not(target_os = "linux"))]
fn dispatch_release_list_request_macos(proxy: &tao::event_loop::EventLoopProxy<TaoUserEvent>) {
    let proxy = proxy.clone();
    std::thread::spawn(move || {
        let state = server_action::request_release_list(RELEASE_LIST_TIMEOUT);
        drop(proxy.send_event(TaoUserEvent::ReleaseListResult(state)));
    });
}

/// macOS: spawn a worker thread to issue an `EnvPreflight` request to the
/// server, then deliver the outcome as a tao user event so the main event
/// loop can call `evaluate_script` on the UI thread. Mirrors
/// [`dispatch_release_list_request_macos`].
#[cfg(not(target_os = "linux"))]
fn dispatch_env_preflight_request_macos(proxy: &tao::event_loop::EventLoopProxy<TaoUserEvent>) {
    let proxy = proxy.clone();
    std::thread::spawn(move || {
        let outcome = server_action::request_env_preflight(ENV_PREFLIGHT_TIMEOUT);
        drop(proxy.send_event(TaoUserEvent::EnvPreflightResult(outcome)));
    });
}

#[cfg(not(target_os = "linux"))]
fn dispatch_workspace_root_picker_macos(proxy: &tao::event_loop::EventLoopProxy<TaoUserEvent>) {
    let proxy = proxy.clone();
    std::thread::spawn(move || {
        let output = std::process::Command::new("osascript")
            .args(["-e", r#"POSIX path of (choose folder with prompt "Choose workspace root")"#])
            .output();

        match output {
            Ok(output) if output.status.success() => {
                let path = String::from_utf8_lossy(&output.stdout).trim().to_owned();
                if !path.is_empty() {
                    drop(proxy.send_event(TaoUserEvent::WorkspaceRootChosen(path)));
                }
            }
            Ok(output) => {
                let reason = String::from_utf8_lossy(&output.stderr);
                tracing::debug!("workspace root picker cancelled or failed: {}", reason.trim());
            }
            Err(e) => tracing::warn!("failed to launch workspace root picker: {e}"),
        }
    });
}

#[cfg(target_os = "macos")]
fn open_macos_notification_settings() {
    let url = "x-apple.systempreferences:com.apple.preference.notifications";
    if let Err(e) = std::process::Command::new("open").arg(url).spawn() {
        tracing::warn!("failed to open macOS notification settings: {e}");
    }
}

#[cfg(not(target_os = "macos"))]
fn open_macos_notification_settings() {}

/// Handle a stateless platform action issued by the webview.
///
/// Returns `true` when the action was recognised and handled, `false`
/// otherwise. Stateful actions (font refresh, manual update check, manual
/// release list, config writes) are handled directly in
/// [`handle_settings_ipc_request`] because they need access to the webview
/// reference or caller closures.
fn handle_settings_ipc_action(kind: &str, body: &str) -> bool {
    match kind {
        "open_macos_notification_settings" => {
            open_macos_notification_settings();
            true
        }
        "open_external_url" => {
            if let Some(url) = settings_ipc_request_url(body) {
                dispatch_open_external_url(&url);
            } else {
                tracing::warn!("open_external_url message missing url field");
            }
            true
        }
        _ => false,
    }
}

/// Extract the `url` field from a webview IPC request body. Returns `None`
/// when the field is absent or not a string so callers can log and drop the
/// message instead of dispatching an undefined opener invocation.
fn settings_ipc_request_url(body: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("url").and_then(|t| t.as_str()).map(str::to_owned))
}

#[derive(Clone, Copy)]
struct SettingsIpcHandlers<'a> {
    webview_ref: &'a std::rc::Rc<std::cell::RefCell<Option<wry::WebView>>>,
    on_change: &'a dyn Fn(String),
    on_request_update_check: &'a dyn Fn(),
    on_trigger_update: &'a dyn Fn(),
    on_request_releases: &'a dyn Fn(),
    on_choose_workspace_root: &'a dyn Fn(),
    on_request_env_preflight: &'a dyn Fn(),
}

/// Handle an IPC request from the settings webview.
///
/// Dispatches by `type` field to the appropriate path. The match is structured
/// as an explicit if/else chain (rather than a single `match` block) because
/// the branches differ in what extra state they need: `request_fonts`,
/// `request_update_check`, and `request_releases` need closures captured in
/// the calling scope, while `setting_changed` carries the full body, and the
/// platform-action group is handled by a stateless inner helper.
fn handle_settings_ipc_request(body: &str, handlers: SettingsIpcHandlers<'_>) {
    let Some(kind) = settings_ipc_request_type(body) else {
        tracing::debug!("settings IPC request missing type");
        return;
    };

    if kind == "request_fonts" {
        if let Some(wv) = handlers.webview_ref.borrow().as_ref() {
            inject_font_list(wv);
        }
    } else if kind == "request_update_check" {
        (handlers.on_request_update_check)();
    } else if kind == "trigger_update" {
        (handlers.on_trigger_update)();
    } else if kind == "request_releases" {
        (handlers.on_request_releases)();
    } else if kind == "choose_workspace_root" {
        (handlers.on_choose_workspace_root)();
    } else if kind == "env_preflight" {
        (handlers.on_request_env_preflight)();
    } else if kind == "setting_changed" {
        (handlers.on_change)(body.to_owned());
    } else if !handle_settings_ipc_action(&kind, body) {
        tracing::debug!(kind, "unhandled settings IPC request");
    }
}

#[cfg(target_os = "macos")]
fn is_macos_close_window_shortcut(
    event: &tao::event::KeyEvent,
    modifiers: tao::keyboard::ModifiersState,
) -> bool {
    event.state == tao::event::ElementState::Pressed
        && !event.repeat
        && modifiers.super_key()
        && !modifiers.control_key()
        && !modifiers.alt_key()
        && !modifiers.shift_key()
        && matches!(&event.logical_key, tao::keyboard::Key::Character(ch) if ch.eq_ignore_ascii_case("w"))
}

#[cfg(all(not(target_os = "macos"), not(target_os = "linux")))]
fn is_macos_close_window_shortcut(
    _event: &tao::event::KeyEvent,
    _modifiers: tao::keyboard::ModifiersState,
) -> bool {
    false
}

// ---------------------------------------------------------------------------
// Linux: GTK-based settings window
// ---------------------------------------------------------------------------

pub struct SettingsWindowRunArgs<OnChange, OnClose> {
    pub geometry: Option<SettingsWindowGeometry>,
    pub launch_anchor: Option<SettingsWindowAnchor>,
    pub on_change: OnChange,
    pub on_close: OnClose,
    pub listener: std::os::unix::net::UnixListener,
    pub socket_path: std::path::PathBuf,
}

/// Run the settings window (blocking until closed).
///
/// On Linux, initialises GTK, creates the window, registers the singleton
/// socket fd watcher, installs signal handlers, and enters `gtk::main()`.
///
/// On macOS, this is a stub — the cross-platform implementation is provided
/// by a separate task.
///
/// `on_change` is called for each setting change from the webview.
/// `on_close` is called with the final geometry when the window closes.
#[cfg(target_os = "linux")]
pub fn run_settings_window<OnChange, OnClose>(
    args: SettingsWindowRunArgs<OnChange, OnClose>,
) -> Result<(), String>
where
    OnChange: Fn(String) + 'static,
    OnClose: FnOnce(SettingsWindowGeometry) + 'static,
{
    use std::cell::RefCell;
    use std::rc::Rc;

    use gtk::prelude::*;

    let SettingsWindowRunArgs {
        geometry,
        launch_anchor,
        on_change,
        on_close,
        listener,
        socket_path: _socket_path,
    } = args;

    if let Err(e) = gtk::init() {
        return Err(format!("GTK init failed: {e}"));
    }

    let config_json = load_config_json();
    let html = build_html()?;
    let window = build_linux_window(geometry);

    // Create a GTK Box to hold the webview.
    let container = gtk::Box::new(gtk::Orientation::Vertical, 0);
    window.add(&container);
    window.show_all();
    position_linux_window(&window, geometry, launch_anchor);

    // Shared webview reference so the IPC handler can call evaluate_script
    // for font refresh requests. The webview is stored after build_gtk.
    let ctx = LinuxWebviewContext {
        webview_ref: Rc::new(RefCell::new(None)),
        active_check_source: new_active_check_source(),
        active_release_source: new_active_check_source(),
        active_env_preflight_source: new_active_check_source(),
    };
    let webview = build_linux_webview(
        LinuxWebviewBuild {
            window: &window,
            container: &container,
            html: &html,
            config_json: &config_json,
            ctx: &ctx,
        },
        on_change,
    )?;

    // Store webview in the shared ref so the IPC handler can use it for refresh.
    *ctx.webview_ref.borrow_mut() = Some(webview);

    // Wrap on_close in an Rc<RefCell<Option<...>>> so it can be shared
    // between the delete-event handler and the SIGTERM/SIGINT signal handlers.
    // Each handler calls take() to fire the callback exactly once.
    let on_close = Rc::new(RefCell::new(Some(on_close)));
    install_linux_runtime_hooks(&window, listener, Rc::clone(&on_close));

    gtk::main();

    // Drop any in-flight manual-check, release-list, or env-preflight timeout
    // source before the webview is dropped, so the closure releases its
    // `Rc<WebView>` before the underlying widget tears down.
    cancel_active_check_source(&ctx.active_check_source);
    cancel_active_check_source(&ctx.active_release_source);
    cancel_active_check_source(&ctx.active_env_preflight_source);

    Ok(())
}

/// Position the Linux settings window after it is visible.
#[cfg(target_os = "linux")]
fn position_linux_window(
    window: &gtk::Window,
    geometry: Option<SettingsWindowGeometry>,
    launch_anchor: Option<SettingsWindowAnchor>,
) {
    use gtk::prelude::GtkWindowExt;

    // GTK3 docs note that most window managers ignore position requests for
    // unmapped windows but honour move() once the window is visible. On
    // Wayland, move() is a protocol-level no-op for toplevel windows.
    if is_wayland_backend() {
        return;
    }

    if let Some(anchor) = launch_anchor {
        move_linux_window_to_anchor(window, anchor);
        raise_linux_window_above_launcher(window);
        return;
    }

    if let Some(geom) = geometry {
        if saved_geometry_intersects_current_monitor(geom) {
            window.move_(geom.x, geom.y);
        } else {
            tracing::info!(
                x = geom.x,
                y = geom.y,
                width = geom.width,
                height = geom.height,
                "saved settings window position is off-screen, letting window manager place it"
            );
        }
    }
}

/// Build the GTK window with the saved geometry and app icon.
#[cfg(target_os = "linux")]
fn build_linux_window(geometry: Option<SettingsWindowGeometry>) -> gtk::Window {
    use gtk::prelude::*;

    // Set the window icon so the taskbar shows the correct app icon.
    // set_default_icon_name alone is not enough — some panels match by
    // WM_CLASS and ignore the theme name. Loading a pixbuf from the installed
    // icon file and setting it directly on the window embeds the icon in
    // _NET_WM_ICON, which all panels respect.
    let icon_name = scribe_common::app::current_identity().slug();
    gtk::Window::set_default_icon_name(icon_name);

    let window = gtk::Window::new(gtk::WindowType::Toplevel);
    let window_title =
        format!("{} Settings", scribe_common::app::current_identity().window_title_name());
    window.set_title(&window_title);

    let icon_path = format!("/usr/share/icons/hicolor/256x256/apps/{icon_name}.png");
    match gdk_pixbuf::Pixbuf::from_file(&icon_path) {
        Ok(pixbuf) => window.set_icon(Some(&pixbuf)),
        Err(e) => tracing::warn!("failed to load window icon from {icon_path}: {e}"),
    }

    if let Some(geom) = geometry {
        window.set_default_size(geom.width, geom.height);
    } else {
        window.set_default_size(880, 680);
        window.set_position(gtk::WindowPosition::Center);
    }

    window
}

/// Move the GTK settings window so it is centered over the launcher terminal.
#[cfg(target_os = "linux")]
fn move_linux_window_to_anchor(window: &gtk::Window, anchor: SettingsWindowAnchor) {
    use gtk::prelude::GtkWindowExt;

    if !anchor.is_sane() || is_wayland_backend() {
        return;
    }

    let (width, height) = window.size();
    let (x, y) = centered_settings_position(anchor, width, height);
    let (x, y) = anchor_workarea(anchor)
        .map_or((x, y), |area| clamp_position_to_rect(x, y, width, height, &area));
    window.move_(x, y);
}

/// Raise the GTK settings window above the launcher terminal.
///
/// `gtk_window_present()` is documented as broken for cross-process raises:
/// the WM has no fresh user-input timestamp from the settings process, so
/// X11 focus-stealing prevention silently demotes the raise to "demand
/// attention" and the window appears behind the launcher. Fetching
/// `gdk_x11_get_server_time` does a cheap X round-trip to obtain a timestamp
/// the WM accepts as "happening now", so `present_with_time` raises the
/// window above the launcher reliably.
#[cfg(target_os = "linux")]
fn raise_linux_window_above_launcher(window: &gtk::Window) {
    use gdk::prelude::Cast;
    use gtk::prelude::*;

    if is_wayland_backend() {
        window.present();
        return;
    }

    let timestamp = window
        .window()
        .and_then(|gdk_window| gdk_window.downcast::<gdkx11::X11Window>().ok())
        .map(|x11_window| gdkx11::functions::x11_get_server_time(&x11_window));

    match timestamp {
        Some(t) => window.present_with_time(t),
        None => window.present(),
    }
}

#[cfg(target_os = "linux")]
fn anchor_workarea(anchor: SettingsWindowAnchor) -> Option<gdk::Rectangle> {
    use gdk::prelude::MonitorExt;

    let display = gdk::Display::default()?;
    let center_x = i64_to_i32_saturating(i64::from(anchor.x) + i64::from(anchor.width) / 2);
    let center_y = i64_to_i32_saturating(i64::from(anchor.y) + i64::from(anchor.height) / 2);
    let monitor =
        display.monitor_at_point(center_x, center_y).or_else(|| display.primary_monitor())?;
    Some(monitor.workarea())
}

#[cfg(target_os = "linux")]
fn saved_geometry_intersects_current_monitor(geom: SettingsWindowGeometry) -> bool {
    use gdk::prelude::MonitorExt;

    let Some(display) = gdk::Display::default() else {
        return true;
    };
    let rect = gdk::Rectangle::new(geom.x, geom.y, geom.width.max(1), geom.height.max(1));
    for idx in 0..display.n_monitors() {
        let Some(monitor) = display.monitor(idx) else { continue };
        if monitor.workarea().intersect(&rect).is_some() {
            return true;
        }
    }
    false
}

#[cfg(target_os = "linux")]
fn clamp_position_to_rect(
    x: i32,
    y: i32,
    window_width: i32,
    window_height: i32,
    rect: &gdk::Rectangle,
) -> (i32, i32) {
    (
        clamp_axis(x, rect.x(), rect.width(), window_width),
        clamp_axis(y, rect.y(), rect.height(), window_height),
    )
}

#[cfg(target_os = "linux")]
fn clamp_axis(position: i32, rect_origin: i32, rect_size: i32, window_size: i32) -> i32 {
    let max = if rect_size > window_size {
        rect_origin.saturating_add(rect_size - window_size)
    } else {
        rect_origin
    };
    position.clamp(rect_origin, max)
}

#[cfg(target_os = "linux")]
fn i64_to_i32_saturating(value: i64) -> i32 {
    i32::try_from(value).unwrap_or_else(|_| if value.is_negative() { i32::MIN } else { i32::MAX })
}

/// Webview-side state shared between the IPC handler and the window shutdown
/// path on Linux: the webview reference itself plus the cells tracking any
/// in-flight manual-check / release-list timeout sources.
#[cfg(target_os = "linux")]
struct LinuxWebviewContext {
    webview_ref: std::rc::Rc<std::cell::RefCell<Option<wry::WebView>>>,
    active_check_source: ActiveCheckSource,
    active_release_source: ActiveCheckSource,
    active_env_preflight_source: ActiveCheckSource,
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy)]
struct LinuxWebviewBuild<'a> {
    window: &'a gtk::Window,
    container: &'a gtk::Box,
    html: &'a str,
    config_json: &'a str,
    ctx: &'a LinuxWebviewContext,
}

/// Build the GTK webview and wire the settings IPC handler.
#[cfg(target_os = "linux")]
fn build_linux_webview<F: Fn(String) + 'static>(
    build: LinuxWebviewBuild<'_>,
    on_change: F,
) -> Result<wry::WebView, String> {
    use wry::WebViewBuilderExtUnix;

    let LinuxWebviewBuild { window, container, html, config_json, ctx } = build;
    let bootstrap = bootstrap_script(env!("CARGO_PKG_VERSION"), current_platform_string());
    let webview = wry::WebViewBuilder::new()
        .with_initialization_script(bootstrap)
        .with_html(html)
        .with_ipc_handler({
            let webview_for_ipc = std::rc::Rc::clone(&ctx.webview_ref);
            let webview_for_check = std::rc::Rc::clone(&ctx.webview_ref);
            let active_for_check = std::rc::Rc::clone(&ctx.active_check_source);
            let on_request_update_check = move || {
                dispatch_check_for_updates_linux(&webview_for_check, &active_for_check);
            };
            let on_trigger_update = || dispatch_trigger_update();
            let webview_for_releases = std::rc::Rc::clone(&ctx.webview_ref);
            let active_for_releases = std::rc::Rc::clone(&ctx.active_release_source);
            let on_request_releases = move || {
                dispatch_release_list_request_linux(&webview_for_releases, &active_for_releases);
            };
            let webview_for_env = std::rc::Rc::clone(&ctx.webview_ref);
            let active_for_env = std::rc::Rc::clone(&ctx.active_env_preflight_source);
            let on_request_env_preflight = move || {
                dispatch_env_preflight_request_linux(&webview_for_env, &active_for_env);
            };
            let window_for_picker = window.clone();
            let webview_for_picker = std::rc::Rc::clone(&ctx.webview_ref);
            let on_choose_workspace_root = move || {
                dispatch_workspace_root_picker_linux(&window_for_picker, &webview_for_picker);
            };
            move |request| {
                handle_settings_ipc_request(
                    request.body(),
                    SettingsIpcHandlers {
                        webview_ref: &webview_for_ipc,
                        on_change: &on_change,
                        on_request_update_check: &on_request_update_check,
                        on_trigger_update: &on_trigger_update,
                        on_request_releases: &on_request_releases,
                        on_choose_workspace_root: &on_choose_workspace_root,
                        on_request_env_preflight: &on_request_env_preflight,
                    },
                );
            }
        })
        .build_gtk(container)
        .map_err(|e| format!("failed to create webview: {e}"))?;

    inject_initial_webview_state(&webview, config_json);
    Ok(webview)
}

#[cfg(target_os = "linux")]
fn dispatch_workspace_root_picker_linux(
    window: &gtk::Window,
    webview_ref: &std::rc::Rc<std::cell::RefCell<Option<wry::WebView>>>,
) {
    use gtk::prelude::*;

    let dialog = gtk::FileChooserDialog::with_buttons(
        Some("Choose Workspace Root"),
        Some(window),
        gtk::FileChooserAction::SelectFolder,
        &[("Cancel", gtk::ResponseType::Cancel), ("Choose", gtk::ResponseType::Accept)],
    );

    let selected = if dialog.run() == gtk::ResponseType::Accept { dialog.filename() } else { None };
    dialog.close();

    if let Some(path) = selected.and_then(|path| path.into_os_string().into_string().ok()) {
        inject_workspace_root_choice(webview_ref, &path);
    }
}

/// Install the Linux singleton watcher and shutdown handlers.
#[cfg(target_os = "linux")]
fn install_linux_runtime_hooks<F: FnOnce(SettingsWindowGeometry) + 'static>(
    window: &gtk::Window,
    listener: std::os::unix::net::UnixListener,
    on_close: std::rc::Rc<std::cell::RefCell<Option<F>>>,
) {
    install_linux_singleton_watcher(listener, window);
    install_linux_signal_handler(libc::SIGTERM, window, std::rc::Rc::clone(&on_close));
    install_linux_signal_handler(libc::SIGINT, window, std::rc::Rc::clone(&on_close));
    install_linux_delete_handler(window, on_close);
}

/// Watch the singleton socket for incoming focus commands.
#[cfg(target_os = "linux")]
fn install_linux_singleton_watcher(
    listener: std::os::unix::net::UnixListener,
    window: &gtk::Window,
) {
    let fd = std::os::unix::io::AsRawFd::as_raw_fd(&listener);
    let window_for_focus = window.clone();
    gtk::glib::unix_fd_add_local(fd, gtk::glib::IOCondition::IN, move |_, _| {
        handle_singleton_connection(&listener, &window_for_focus);
        gtk::glib::ControlFlow::Continue
    });
}

/// Register one Linux termination signal handler.
#[cfg(target_os = "linux")]
fn install_linux_signal_handler<F: FnOnce(SettingsWindowGeometry) + 'static>(
    signal: libc::c_int,
    window: &gtk::Window,
    on_close: std::rc::Rc<std::cell::RefCell<Option<F>>>,
) {
    let window_for_signal = window.clone();
    gtk::glib::unix_signal_add_local(signal, move || {
        fire_linux_on_close(&window_for_signal, &on_close);
        gtk::main_quit();
        gtk::glib::ControlFlow::Break
    });
}

/// Register the Linux window delete handler.
#[cfg(target_os = "linux")]
fn install_linux_delete_handler<F: FnOnce(SettingsWindowGeometry) + 'static>(
    window: &gtk::Window,
    on_close: std::rc::Rc<std::cell::RefCell<Option<F>>>,
) {
    use gtk::prelude::WidgetExt;

    window.connect_delete_event(move |win, _| {
        fire_linux_on_close(win, &on_close);
        gtk::main_quit();
        gtk::glib::Propagation::Proceed
    });
}

/// Fire the Linux close callback exactly once with the current geometry.
#[cfg(target_os = "linux")]
fn fire_linux_on_close<F: FnOnce(SettingsWindowGeometry)>(
    window: &gtk::Window,
    on_close: &std::rc::Rc<std::cell::RefCell<Option<F>>>,
) {
    use gtk::prelude::GtkWindowExt;

    let (x, y) = window.position();
    let (width, height) = window.size();
    if let Some(cb) = on_close.borrow_mut().take() {
        cb(SettingsWindowGeometry { x, y, width, height });
    }
}

// ---------------------------------------------------------------------------
// macOS: tao + wry settings window
// ---------------------------------------------------------------------------

/// Custom event for the tao event loop.
#[cfg(not(target_os = "linux"))]
enum TaoUserEvent {
    /// Another instance sent a "focus" command via the singleton socket.
    FocusWindow(Option<SettingsWindowAnchor>),
    /// App shutdown requested over the singleton socket; preserve open state
    /// so a fresh Scribe launch can restore the settings window.
    QuitWindow,
    /// A termination signal (SIGTERM/SIGINT) was received.
    Terminate,
    /// A worker thread finished a manual update check; deliver the result to
    /// the webview on the main thread.
    UpdateCheckResult(UpdateCheckResultState),
    /// A worker thread finished a manual release-list request; deliver the
    /// result to the webview on the main thread.
    ReleaseListResult(ReleaseListResultState),
    /// A worker thread finished an env-persistence preflight request;
    /// deliver the outcome to the webview on the main thread so the toggle
    /// can commit or surface the inline error.
    EnvPreflightResult(server_action::EnvPreflightOutcome),
    /// A native directory picker returned a workspace root path.
    WorkspaceRootChosen(String),
}

/// Build the tao window with optional saved geometry.
#[cfg(not(target_os = "linux"))]
fn build_tao_window(
    event_loop: &tao::event_loop::EventLoop<TaoUserEvent>,
    geometry: Option<SettingsWindowGeometry>,
    launch_anchor: Option<SettingsWindowAnchor>,
) -> Result<tao::window::Window, String> {
    use tao::dpi::{LogicalPosition, LogicalSize};

    let window_title =
        format!("{} Settings", scribe_common::app::current_identity().window_title_name());
    let mut builder = tao::window::WindowBuilder::new().with_title(&window_title);

    if let Some(geom) = geometry {
        builder = builder
            .with_inner_size(LogicalSize::new(f64::from(geom.width), f64::from(geom.height)));
        if launch_anchor.is_none() {
            builder =
                builder.with_position(LogicalPosition::new(f64::from(geom.x), f64::from(geom.y)));
        }
    } else {
        builder = builder.with_inner_size(LogicalSize::new(880.0, 680.0));
    }

    builder.build(event_loop).map_err(|e| format!("failed to create window: {e}"))
}

/// Spawn a background thread to accept singleton socket connections.
///
/// When a valid "focus" command arrives, sends `FocusWindow` to the event loop.
#[cfg(not(target_os = "linux"))]
fn spawn_singleton_listener(
    listener: std::os::unix::net::UnixListener,
    proxy: tao::event_loop::EventLoopProxy<TaoUserEvent>,
) {
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            if !singleton::verify_peer_uid(&stream) {
                continue;
            }
            match singleton::read_command(&stream) {
                Some(command) if command.cmd == "focus" => {
                    drop(proxy.send_event(TaoUserEvent::FocusWindow(command.anchor)));
                }
                Some(command) if command.cmd == "quit" => {
                    drop(proxy.send_event(TaoUserEvent::QuitWindow))
                }
                _ => {}
            }
        }
    });
}

/// Register SIGTERM/SIGINT handlers that send `Terminate` to the event loop.
#[cfg(not(target_os = "linux"))]
fn register_signal_handlers(proxy: tao::event_loop::EventLoopProxy<TaoUserEvent>) {
    use signal_hook::consts::{SIGINT, SIGTERM};
    use signal_hook::iterator::Signals;

    let Ok(mut signals) = Signals::new([SIGTERM, SIGINT]) else {
        tracing::warn!("failed to register signal handlers");
        return;
    };

    std::thread::spawn(move || {
        if signals.into_iter().next().is_some() {
            drop(proxy.send_event(TaoUserEvent::Terminate));
        }
    });
}

/// Capture the current window geometry as a `SettingsWindowGeometry`.
#[cfg(not(target_os = "linux"))]
fn capture_geometry(window: &tao::window::Window) -> SettingsWindowGeometry {
    let pos = window.outer_position().unwrap_or_default();
    let size = window.inner_size();
    SettingsWindowGeometry {
        x: pos.x,
        y: pos.y,
        width: size.width.cast_signed(),
        height: size.height.cast_signed(),
    }
}

/// Move the tao settings window so it is centered over the launcher terminal.
#[cfg(not(target_os = "linux"))]
fn move_tao_window_to_anchor(window: &tao::window::Window, anchor: SettingsWindowAnchor) {
    if !anchor.is_sane() {
        return;
    }

    let size = window.outer_size();
    let width = i32::try_from(size.width).unwrap_or(i32::MAX);
    let height = i32::try_from(size.height).unwrap_or(i32::MAX);
    let (x, y) = centered_settings_position(anchor, width, height);
    window.set_outer_position(tao::dpi::PhysicalPosition::new(x, y));
}

/// Run the settings window on macOS using tao + wry (blocking until closed).
///
/// Uses `tao::EventLoop` for windowing and `wry::WebViewBuilder::build()`
/// with the tao window (no GTK dependency).
#[cfg(not(target_os = "linux"))]
pub fn run_settings_window<OnChange, OnClose>(
    args: SettingsWindowRunArgs<OnChange, OnClose>,
) -> Result<(), String>
where
    OnChange: Fn(String) + 'static,
    OnClose: FnOnce(SettingsWindowGeometry) + 'static,
{
    use std::cell::RefCell;
    use std::rc::Rc;

    let SettingsWindowRunArgs {
        geometry,
        launch_anchor,
        on_change,
        on_close,
        listener,
        socket_path: _socket_path,
    } = args;

    let config_json = load_config_json();
    let html = build_html()?;

    let mut event_loop =
        tao::event_loop::EventLoopBuilder::<TaoUserEvent>::with_user_event().build();
    let window = build_tao_window(&event_loop, geometry, launch_anchor)?;
    if let Some(anchor) = launch_anchor {
        move_tao_window_to_anchor(&window, anchor);
    }

    // Spawn singleton listener and signal handlers on background threads.
    spawn_singleton_listener(listener, event_loop.create_proxy());
    register_signal_handlers(event_loop.create_proxy());

    // Shared webview ref for IPC font-refresh requests.
    let webview_ref: Rc<RefCell<Option<wry::WebView>>> = Rc::new(RefCell::new(None));
    let webview = build_tao_webview(
        &window,
        &html,
        &config_json,
        on_change,
        &webview_ref,
        event_loop.create_proxy(),
    )?;

    *webview_ref.borrow_mut() = Some(webview);

    run_tao_settings_loop(
        &mut event_loop,
        window,
        Rc::clone(&webview_ref),
        RefCell::new(Some(on_close)),
    );

    Ok(())
}

/// Fire the `on_close` callback exactly once, capturing current geometry.
#[cfg(not(target_os = "linux"))]
fn fire_on_close<F: FnOnce(SettingsWindowGeometry)>(
    window: &tao::window::Window,
    on_close: &std::cell::RefCell<Option<F>>,
) {
    if let Some(cb) = on_close.borrow_mut().take() {
        cb(capture_geometry(window));
    }
}

/// Build the tao webview and wire the settings IPC handler.
#[cfg(not(target_os = "linux"))]
fn build_tao_webview<F: Fn(String) + 'static>(
    window: &tao::window::Window,
    html: &str,
    config_json: &str,
    on_change: F,
    webview_ref: &std::rc::Rc<std::cell::RefCell<Option<wry::WebView>>>,
    event_loop_proxy: tao::event_loop::EventLoopProxy<TaoUserEvent>,
) -> Result<wry::WebView, String> {
    let bootstrap = bootstrap_script(env!("CARGO_PKG_VERSION"), current_platform_string());
    let webview = wry::WebViewBuilder::new()
        .with_initialization_script(bootstrap)
        .with_html(html)
        .with_ipc_handler({
            let webview_for_ipc = std::rc::Rc::clone(webview_ref);
            let proxy_for_check = event_loop_proxy.clone();
            let proxy_for_releases = event_loop_proxy.clone();
            let proxy_for_env = event_loop_proxy.clone();
            let proxy_for_workspace_root = event_loop_proxy.clone();
            let on_request_update_check =
                move || dispatch_check_for_updates_macos(&proxy_for_check);
            let on_trigger_update = || dispatch_trigger_update();
            let on_request_releases =
                move || dispatch_release_list_request_macos(&proxy_for_releases);
            let on_request_env_preflight =
                move || dispatch_env_preflight_request_macos(&proxy_for_env);
            let on_choose_workspace_root =
                move || dispatch_workspace_root_picker_macos(&proxy_for_workspace_root);
            move |request| {
                handle_settings_ipc_request(
                    request.body(),
                    SettingsIpcHandlers {
                        webview_ref: &webview_for_ipc,
                        on_change: &on_change,
                        on_request_update_check: &on_request_update_check,
                        on_trigger_update: &on_trigger_update,
                        on_request_releases: &on_request_releases,
                        on_choose_workspace_root: &on_choose_workspace_root,
                        on_request_env_preflight: &on_request_env_preflight,
                    },
                );
            }
        })
        .build(window)
        .map_err(|e| format!("failed to create webview: {e}"))?;

    inject_initial_webview_state(&webview, config_json);
    Ok(webview)
}

/// Run the tao event loop for the settings window.
#[cfg(not(target_os = "linux"))]
fn run_tao_settings_loop<F: FnOnce(SettingsWindowGeometry) + 'static>(
    event_loop: &mut tao::event_loop::EventLoop<TaoUserEvent>,
    window: tao::window::Window,
    webview_ref: std::rc::Rc<std::cell::RefCell<Option<wry::WebView>>>,
    on_close: std::cell::RefCell<Option<F>>,
) {
    use tao::event_loop::ControlFlow;
    use tao::platform::run_return::EventLoopExtRunReturn;

    let target_window_id = window.id();
    let modifiers = std::cell::RefCell::new(tao::keyboard::ModifiersState::default());

    event_loop.run_return(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        handle_tao_event(
            event,
            &window,
            &webview_ref,
            &on_close,
            &modifiers,
            target_window_id,
            control_flow,
        );
    });
}

/// Handle a single tao event for the settings window.
#[cfg(not(target_os = "linux"))]
fn handle_tao_event<F: FnOnce(SettingsWindowGeometry)>(
    event: tao::event::Event<TaoUserEvent>,
    window: &tao::window::Window,
    webview_ref: &std::rc::Rc<std::cell::RefCell<Option<wry::WebView>>>,
    on_close: &std::cell::RefCell<Option<F>>,
    modifiers: &std::cell::RefCell<tao::keyboard::ModifiersState>,
    target_window_id: tao::window::WindowId,
    control_flow: &mut tao::event_loop::ControlFlow,
) {
    use tao::event::Event;
    use tao::event::WindowEvent;
    use tao::event_loop::ControlFlow;

    match event {
        Event::UserEvent(TaoUserEvent::FocusWindow(anchor)) => {
            if let Some(anchor) = anchor {
                move_tao_window_to_anchor(window, anchor);
            }
            window.set_focus();
        }
        Event::UserEvent(TaoUserEvent::QuitWindow) => {
            *control_flow = ControlFlow::Exit;
        }
        Event::UserEvent(TaoUserEvent::UpdateCheckResult(state)) => {
            inject_update_check_result(webview_ref, &state);
        }
        Event::UserEvent(TaoUserEvent::ReleaseListResult(state)) => {
            inject_release_list_result(webview_ref, &state);
        }
        Event::UserEvent(TaoUserEvent::EnvPreflightResult(outcome)) => {
            inject_env_preflight_result(webview_ref, &outcome);
        }
        Event::UserEvent(TaoUserEvent::WorkspaceRootChosen(path)) => {
            inject_workspace_root_choice(webview_ref, &path);
        }
        Event::WindowEvent {
            event: WindowEvent::ModifiersChanged(new_mods),
            window_id: id,
            ..
        } if id == target_window_id => {
            *modifiers.borrow_mut() = new_mods;
        }
        Event::WindowEvent {
            event: WindowEvent::KeyboardInput { event, .. },
            window_id: id,
            ..
        } if id == target_window_id
            && is_macos_close_window_shortcut(&event, *modifiers.borrow()) =>
        {
            fire_on_close(window, on_close);
            *control_flow = ControlFlow::Exit;
        }
        Event::WindowEvent { event: WindowEvent::CloseRequested, .. }
        | Event::UserEvent(TaoUserEvent::Terminate) => {
            fire_on_close(window, on_close);
            *control_flow = ControlFlow::Exit;
        }
        Event::WindowEvent { event: WindowEvent::Destroyed, window_id: id, .. }
            if id == target_window_id =>
        {
            *control_flow = ControlFlow::Exit;
        }
        _ => {}
    }
}

/// Handle one incoming connection on the singleton socket.
///
/// Accepts a connection, verifies peer UID, reads the command, and
/// presents the window if the command is `"focus"`.
#[cfg(target_os = "linux")]
fn handle_singleton_connection(listener: &std::os::unix::net::UnixListener, window: &gtk::Window) {
    let Ok((stream, _)) = listener.accept() else {
        return;
    };
    if !singleton::verify_peer_uid(&stream) {
        return;
    }
    match singleton::read_command(&stream) {
        Some(command) if command.cmd == "focus" => {
            if let Some(anchor) = command.anchor {
                move_linux_window_to_anchor(window, anchor);
            }
            raise_linux_window_above_launcher(window);
        }
        Some(command) if command.cmd == "quit" => gtk::main_quit(),
        _ => {}
    }
}

/// Check whether GTK is using the Wayland backend.
///
/// On Wayland, `gtk::Window::position()` always returns `(0, 0)` and
/// `move_()` is a protocol-level no-op for toplevel windows — position
/// save/restore is not possible.
#[cfg(target_os = "linux")]
fn is_wayland_backend() -> bool {
    // GDK_BACKEND=x11 forces X11 even on a Wayland session.
    match std::env::var("GDK_BACKEND").ok().as_deref() {
        Some("x11") => false,
        Some("wayland") => true,
        // GTK3 auto-selects Wayland when the compositor is running.
        _ => std::env::var_os("WAYLAND_DISPLAY").is_some(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bootstrap script must include the supplied version verbatim as a
    /// JSON-escaped string literal so the webview can read it as
    /// `window.SCRIBE_BOOTSTRAP.version` before the page loads.
    #[test]
    fn bootstrap_script_contains_version_and_platform() {
        let script = bootstrap_script("9.9.9", "linux");
        assert!(script.contains("window.SCRIBE_BOOTSTRAP"), "script: {script}");
        assert!(script.contains("version: \"9.9.9\""), "script: {script}");
        assert!(script.contains("platform: \"linux\""), "script: {script}");
    }

    /// The current workspace `CARGO_PKG_VERSION` must round-trip through the
    /// bootstrap helper without mutation.
    #[test]
    fn bootstrap_script_uses_workspace_version() {
        let version = env!("CARGO_PKG_VERSION");
        let script = bootstrap_script(version, "linux");
        let expected = format!("version: \"{version}\"");
        assert!(script.contains(&expected), "script: {script}\nexpected: {expected}");
    }

    /// Embedded double-quotes in the version string must be JSON-escaped, not
    /// interpolated raw — otherwise the resulting script would have unbalanced
    /// quotes and break the webview.
    #[test]
    fn bootstrap_script_escapes_embedded_quotes() {
        let script = bootstrap_script("1.0.0\"; alert(1);//", "linux");
        // The literal closing quote of the version field must remain matched
        // (escape sequence kept the inner quote inert).
        assert!(script.contains("version: \"1.0.0\\\"; alert(1);//\""), "script: {script}");
    }

    // -----------------------------------------------------------------------
    // T007: scheme validation for the host-side `open_external_url` handler.
    // The host must accept only http(s) URLs; any other scheme (or no scheme
    // at all) must be rejected so the renderer cannot coerce the host into
    // launching `xdg-open file:///etc/passwd` or `open javascript:…`.
    // -----------------------------------------------------------------------

    #[test]
    fn external_url_is_safe_accepts_http_and_https() {
        assert!(external_url_is_safe("http://example.com"), "plain http must be allowed");
        assert!(external_url_is_safe("https://example.com"), "plain https must be allowed");
        assert!(
            external_url_is_safe("HTTPS://example.com"),
            "scheme check must be case-insensitive"
        );
        assert!(
            external_url_is_safe("HtTp://example.com/path?q=1#frag"),
            "mixed-case http with path/query/fragment must be allowed"
        );
        assert!(
            external_url_is_safe("https://github.com/sharaf-nassar/scribe/releases/tag/v0.4.2"),
            "the canonical 'View on GitHub' shape must be allowed"
        );
    }

    #[test]
    fn external_url_is_safe_rejects_dangerous_schemes() {
        for url in [
            "javascript:alert(1)",
            "JAVASCRIPT:alert(1)",
            "file:///etc/passwd",
            "data:text/html,<script>alert(1)</script>",
            "vbscript:msgbox(1)",
            "ftp://example.com/x",
            "",
            "www.example.com",
            " https://example.com",
        ] {
            assert!(!external_url_is_safe(url), "non-http(s) input {url:?} must be rejected");
        }
    }

    /// `format_iso_utc` must produce a stable RFC 3339 / ISO 8601 UTC literal
    /// for the canonical wave date. Pinning the function against a known epoch
    /// ensures any future arithmetic regression in `unix_seconds_to_utc`
    /// trips this test instead of silently desynchronising the panel UI.
    #[test]
    fn format_iso_utc_emits_known_timestamp() {
        let t = UNIX_EPOCH + Duration::from_secs(1_778_420_709);
        assert_eq!(format_iso_utc(t), "2026-05-10T13:45:09Z");
    }

    /// `format_iso_utc` must agree on the Unix epoch boundary and on a leap
    /// day (2020-02-29) — leap-year handling is the easiest place for a
    /// home-rolled date arithmetic to get wrong.
    #[test]
    fn format_iso_utc_handles_epoch_and_leap_day() {
        assert_eq!(format_iso_utc(UNIX_EPOCH), "1970-01-01T00:00:00Z");
        let leap = UNIX_EPOCH + Duration::from_secs(1_582_934_400);
        assert_eq!(format_iso_utc(leap), "2020-02-29T00:00:00Z");
    }

    /// The release-list payload mirrors `contracts/releases-protocol.md` §2.2:
    /// a flat object with a lower-case `state` discriminator and conditional
    /// `releases`, `reason`, and `fetched_at` siblings.
    #[test]
    fn release_list_payload_json_matches_contract() {
        use scribe_common::protocol::Release;

        let release = Release {
            version: "0.4.2".to_owned(),
            name: Some("0.4.2".to_owned()),
            published_at: "2026-05-09T10:00:00Z".to_owned(),
            body_html: "<p>x</p>".to_owned(),
            prerelease: false,
            html_url: "https://github.com/sharaf-nassar/scribe/releases/tag/v0.4.2".to_owned(),
        };
        let fetched_at = UNIX_EPOCH + Duration::from_secs(1_778_420_709);

        // Fresh: state, releases, fetched_at; no reason.
        let fresh_json = release_list_payload_json(
            &ReleaseListResultState::Fresh { releases: vec![release.clone()] },
            fetched_at,
        )
        .expect("Fresh payload must serialize");
        let fresh: serde_json::Value =
            serde_json::from_str(&fresh_json).expect("Fresh payload is JSON");
        assert_eq!(fresh["state"], "fresh");
        assert_eq!(fresh["fetched_at"], "2026-05-10T13:45:09Z");
        assert!(fresh["releases"].is_array(), "Fresh must carry the releases array");
        assert!(fresh.get("reason").is_none(), "Fresh must not carry reason");

        // Stale: state, releases, reason, fetched_at.
        let stale_json = release_list_payload_json(
            &ReleaseListResultState::Stale {
                releases: vec![release],
                reason: "GitHub unreachable".to_owned(),
            },
            fetched_at,
        )
        .expect("Stale payload must serialize");
        let stale: serde_json::Value =
            serde_json::from_str(&stale_json).expect("Stale payload is JSON");
        assert_eq!(stale["state"], "stale");
        assert_eq!(stale["reason"], "GitHub unreachable");
        assert!(stale["releases"].is_array(), "Stale must carry the releases array");
        assert!(stale.get("fetched_at").is_some(), "Stale must carry fetched_at");

        // Failed: state and reason only.
        let failed_json = release_list_payload_json(
            &ReleaseListResultState::Failed { reason: "rate limited".to_owned() },
            fetched_at,
        )
        .expect("Failed payload must serialize");
        let failed: serde_json::Value =
            serde_json::from_str(&failed_json).expect("Failed payload is JSON");
        assert_eq!(failed["state"], "failed");
        assert_eq!(failed["reason"], "rate limited");
        assert!(failed.get("releases").is_none(), "Failed must not carry releases");
        assert!(failed.get("fetched_at").is_none(), "Failed must not carry fetched_at");
    }

    /// The env-preflight payload mirrors the T033 contract: a flat object
    /// with a boolean `ok` discriminator and a conditional `error` sibling
    /// whose `type` is the snake-case variant name of `PreflightError`.
    /// `Unknown` additionally carries a non-empty `reason` string.
    ///
    /// Built manually because `PreflightError::Unknown(String)` is a
    /// tuple-variant carrying free-form text — `serde_json` rejects the
    /// internally-tagged tuple-variant shape so the host has to render the
    /// flat `{type, reason?}` JSON itself.
    #[test]
    fn env_preflight_payload_json_matches_contract() {
        // ok=true: no error field at all.
        let ok = env_preflight_payload_json(&server_action::EnvPreflightOutcome::Ok)
            .expect("serialise ok");
        let ok_v: serde_json::Value = serde_json::from_str(&ok).expect("ok payload is JSON");
        assert_eq!(ok_v["ok"], serde_json::Value::Bool(true));
        assert!(ok_v.get("error").is_none(), "ok must not carry an error field");

        // ok=false with structured error: matching snake_case type, no reason.
        for (err, expected_type) in [
            (PreflightError::KeychainLocked, "keychain_locked"),
            (PreflightError::SecretServiceUnavailable, "secret_service_unavailable"),
            (PreflightError::KeystoreAccessDenied, "keystore_access_denied"),
        ] {
            let s = env_preflight_payload_json(&server_action::EnvPreflightOutcome::Err(err))
                .expect("serialise structured err");
            let v: serde_json::Value =
                serde_json::from_str(&s).expect("structured err payload is JSON");
            assert_eq!(v["ok"], serde_json::Value::Bool(false));
            assert_eq!(v["error"]["type"], serde_json::Value::String(expected_type.into()));
            assert!(
                v["error"].get("reason").is_none(),
                "{expected_type} must not carry a reason field"
            );
        }

        // ok=false with Unknown: carries the diagnostic reason verbatim.
        let s = env_preflight_payload_json(&server_action::EnvPreflightOutcome::Err(
            PreflightError::Unknown("d-bus down".into()),
        ))
        .expect("serialise unknown err");
        let v: serde_json::Value = serde_json::from_str(&s).expect("unknown err payload is JSON");
        assert_eq!(v["ok"], serde_json::Value::Bool(false));
        assert_eq!(v["error"]["type"], serde_json::Value::String("unknown".into()));
        assert_eq!(v["error"]["reason"], serde_json::Value::String("d-bus down".into()));
    }
}

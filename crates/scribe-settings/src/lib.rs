#![allow(unsafe_code, reason = "wry webview FFI bindings require unsafe")]

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::rc::Rc;
use std::sync::mpsc;

use rust_embed::Embed;

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

/// Request payload sent to the persistent GTK thread.
struct OpenRequest {
    html: String,
    config_json: String,
    on_change: Box<dyn Fn(String) + Send>,
    geometry: Option<SettingsWindowGeometry>,
    on_close: Box<dyn FnOnce(SettingsWindowGeometry) + Send>,
}

/// Handle to the persistent GTK thread.
///
/// GTK must always be used from the same thread that called `gtk::init()`.
/// This struct owns a long-lived thread and sends "open window" requests
/// to it via a channel, allowing the settings window to be reopened
/// after closing.
pub struct SettingsThread {
    tx: mpsc::Sender<OpenRequest>,
}

impl SettingsThread {
    /// Spawn the persistent GTK thread.
    ///
    /// Call this once at startup. The returned handle is used to open
    /// settings windows via [`SettingsThread::open`].
    pub fn spawn() -> Self {
        let (tx, rx) = mpsc::channel::<OpenRequest>();

        std::thread::spawn(move || {
            gtk_thread_main(rx);
        });

        Self { tx }
    }

    /// Open the settings window (or re-open it after a previous close).
    ///
    /// `config_json` is the current config serialized as JSON.
    /// `on_change` is called on each individual setting change.
    /// `geometry` optionally restores a saved window position and size.
    /// `on_close` is called with the final window geometry when closed.
    ///
    /// Returns `Err` if assets could not be loaded.
    pub fn open(
        &self,
        config_json: String,
        on_change: impl Fn(String) + Send + 'static,
        geometry: Option<SettingsWindowGeometry>,
        on_close: impl FnOnce(SettingsWindowGeometry) + Send + 'static,
    ) -> Result<(), String> {
        let html = build_html()?;

        // If the GTK thread has exited (channel closed), this silently fails.
        // That's acceptable — the user would need to restart the app.
        let _sent = self.tx.send(OpenRequest {
            html,
            config_json,
            on_change: Box::new(on_change),
            geometry,
            on_close: Box::new(on_close),
        });

        Ok(())
    }
}

/// Build a self-contained HTML document by inlining the CSS and JS assets.
///
/// The resulting HTML can be loaded directly via `wry::WebViewBuilder::with_html`.
fn build_html() -> Result<String, String> {
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

/// Main loop for the persistent GTK thread.
///
/// Calls `gtk::init()` once, then blocks on the channel waiting for
/// `OpenRequest` messages. Each request creates a window, runs
/// `gtk::main()`, and loops back after the window is closed.
#[allow(
    clippy::needless_pass_by_value,
    reason = "Receiver is moved into this thread and must be owned for its lifetime"
)]
fn gtk_thread_main(rx: mpsc::Receiver<OpenRequest>) {
    if let Err(e) = gtk::init() {
        tracing::error!("GTK init failed (is WebKitGTK installed?): {e}");
        return;
    }

    while let Ok(req) = rx.recv() {
        if let Err(e) = run_settings_window(req) {
            tracing::warn!("settings window failed: {e}");
        }
    }
}

/// Run the settings window on the GTK thread (blocking until closed).
///
/// Uses GTK + wry `WebViewBuilderExtUnix::build_gtk` for Wayland + X11 support.
fn run_settings_window(req: OpenRequest) -> Result<(), String> {
    use gtk::prelude::*;
    use wry::WebViewBuilderExtUnix;

    let window = gtk::Window::new(gtk::WindowType::Toplevel);
    window.set_title("Scribe Settings");

    // Restore saved size, or use defaults.
    if let Some(geom) = req.geometry {
        window.set_default_size(geom.width, geom.height);
    } else {
        window.set_default_size(880, 680);
        window.set_position(gtk::WindowPosition::Center);
    }

    // Create a GTK Box to hold the webview.
    let container = gtk::Box::new(gtk::Orientation::Vertical, 0);
    window.add(&container);
    window.show_all();

    // Restore saved position after the window is visible. GTK3 docs note
    // that most window managers ignore position requests for unmapped
    // windows but honour move() once the window is visible. On Wayland,
    // move() is a no-op and position() always returns (0, 0), so skip.
    if let Some(geom) = req.geometry {
        if !is_wayland_backend() {
            window.move_(geom.x, geom.y);
        }
    }

    // Shared webview reference so the IPC handler can call evaluate_script
    // for font refresh requests. The webview is stored after build_gtk.
    let webview_ref: Rc<RefCell<Option<wry::WebView>>> = Rc::new(RefCell::new(None));
    let webview_for_ipc = Rc::clone(&webview_ref);

    let on_change = req.on_change;
    let webview = wry::WebViewBuilder::new()
        .with_html(&req.html)
        .with_ipc_handler(move |request| {
            let body = request.body();
            if body.contains("\"type\":\"request_fonts\"") {
                if let Some(wv) = webview_for_ipc.borrow().as_ref() {
                    inject_font_list(wv);
                }
                return;
            }
            on_change(body.clone());
        })
        .build_gtk(&container)
        .map_err(|e| format!("failed to create webview: {e}"))?;

    // Inject current config into the webview after it loads.
    let init_script =
        format!("if (typeof loadConfig === 'function') {{ loadConfig({}); }}", req.config_json);
    if let Err(e) = webview.evaluate_script(&init_script) {
        tracing::warn!("failed to inject config into settings webview: {e}");
    }

    // Inject keybinding defaults so JS can implement reset-to-default.
    inject_keybinding_defaults(&webview);

    // Inject available monospace fonts.
    inject_font_list(&webview);

    // Store webview in the shared ref so the IPC handler can use it for refresh.
    *webview_ref.borrow_mut() = Some(webview);

    // Capture geometry and close the GTK main loop when the window is closed.
    let on_close = RefCell::new(Some(req.on_close));
    window.connect_delete_event(move |win, _| {
        let (x, y) = win.position();
        let (width, height) = win.size();
        if let Some(cb) = on_close.borrow_mut().take() {
            cb(SettingsWindowGeometry { x, y, width, height });
        }
        gtk::main_quit();
        gtk::glib::Propagation::Proceed
    });

    gtk::main();

    Ok(())
}

/// Query the system for installed monospace font families.
///
/// Returns a sorted, deduplicated list of font family names.
fn list_monospace_fonts() -> Vec<String> {
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

/// Check whether GTK is using the Wayland backend.
///
/// On Wayland, `gtk::Window::position()` always returns `(0, 0)` and
/// `move_()` is a protocol-level no-op for toplevel windows — position
/// save/restore is not possible.
fn is_wayland_backend() -> bool {
    // GDK_BACKEND=x11 forces X11 even on a Wayland session.
    match std::env::var("GDK_BACKEND").ok().as_deref() {
        Some("x11") => false,
        Some("wayland") => true,
        // GTK3 auto-selects Wayland when the compositor is running.
        _ => std::env::var_os("WAYLAND_DISPLAY").is_some(),
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

#![allow(unsafe_code, reason = "wry webview FFI bindings require unsafe")]

pub mod apply;
pub mod singleton;
pub mod state;

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::rc::Rc;

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

/// Run the settings window (blocking until closed).
///
/// Initialises GTK, creates the window, registers the singleton socket fd
/// watcher, installs signal handlers, and enters `gtk::main()`.
///
/// `on_change` is called for each setting change from the webview.
/// `on_close` is called with the final geometry when the window closes.
#[allow(
    clippy::too_many_lines,
    reason = "GTK window setup + webview + socket watcher in one function"
)]
pub fn run_settings_window(
    geometry: Option<SettingsWindowGeometry>,
    on_change: impl Fn(String) + 'static,
    on_close: impl FnOnce(SettingsWindowGeometry) + 'static,
    listener: std::os::unix::net::UnixListener,
    _socket_path: std::path::PathBuf,
) -> Result<(), String> {
    use gtk::prelude::*;
    use wry::WebViewBuilderExtUnix;

    if let Err(e) = gtk::init() {
        return Err(format!("GTK init failed: {e}"));
    }

    let config = scribe_common::config::load_config().unwrap_or_else(|e| {
        tracing::warn!("failed to load config: {e}, using defaults");
        scribe_common::config::ScribeConfig::default()
    });
    let config_json = serde_json::to_string(&config).unwrap_or_else(|e| {
        tracing::warn!("failed to serialize config: {e}");
        String::from("{}")
    });

    let html = build_html()?;

    let window = gtk::Window::new(gtk::WindowType::Toplevel);
    window.set_title("Scribe Settings");

    // Restore saved size, or use defaults.
    if let Some(geom) = geometry {
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
    if let Some(geom) = geometry {
        if !is_wayland_backend() {
            window.move_(geom.x, geom.y);
        }
    }

    // Shared webview reference so the IPC handler can call evaluate_script
    // for font refresh requests. The webview is stored after build_gtk.
    let webview_ref: Rc<RefCell<Option<wry::WebView>>> = Rc::new(RefCell::new(None));
    let webview_for_ipc = Rc::clone(&webview_ref);

    let webview = wry::WebViewBuilder::new()
        .with_html(&html)
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
        format!("if (typeof loadConfig === 'function') {{ loadConfig({config_json}); }}");
    if let Err(e) = webview.evaluate_script(&init_script) {
        tracing::warn!("failed to inject config into settings webview: {e}");
    }

    // Inject keybinding defaults so JS can implement reset-to-default.
    inject_keybinding_defaults(&webview);

    // Inject available monospace fonts.
    inject_font_list(&webview);

    // Store webview in the shared ref so the IPC handler can use it for refresh.
    *webview_ref.borrow_mut() = Some(webview);

    // Watch the singleton socket for incoming focus commands.
    let fd = std::os::unix::io::AsRawFd::as_raw_fd(&listener);
    let window_for_focus = window.clone();
    gtk::glib::unix_fd_add_local(fd, gtk::glib::IOCondition::IN, move |_, _| {
        handle_singleton_connection(&listener, &window_for_focus);
        gtk::glib::ControlFlow::Continue
    });

    // SIGTERM/SIGINT handlers via glib (signal-safe, runs in GTK main loop).
    gtk::glib::unix_signal_add_local(libc::SIGTERM, || {
        gtk::main_quit();
        gtk::glib::ControlFlow::Break
    });
    gtk::glib::unix_signal_add_local(libc::SIGINT, || {
        gtk::main_quit();
        gtk::glib::ControlFlow::Break
    });

    let on_close = RefCell::new(Some(on_close));
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

/// Handle one incoming connection on the singleton socket.
///
/// Accepts a connection, verifies peer UID, reads the command, and
/// presents the window if the command is `"focus"`.
fn handle_singleton_connection(listener: &std::os::unix::net::UnixListener, window: &gtk::Window) {
    use gtk::prelude::GtkWindowExt;

    let Ok((stream, _)) = listener.accept() else {
        return;
    };
    if !singleton::verify_peer_uid(&stream) {
        return;
    }
    if singleton::read_command(&stream).as_deref() == Some("focus") {
        window.present();
    }
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

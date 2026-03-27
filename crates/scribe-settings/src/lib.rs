#![allow(unsafe_code, reason = "wry webview FFI bindings require unsafe")]

pub mod apply;
pub mod singleton;
pub mod state;

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
    let platform = if cfg!(target_os = "macos") { "macos" } else { "linux" };
    let script = format!("window.SCRIBE_PLATFORM = \"{platform}\";");
    if let Err(e) = webview.evaluate_script(&script) {
        tracing::warn!("failed to inject platform into settings webview: {e}");
    }
}

// ---------------------------------------------------------------------------
// Linux: GTK-based settings window
// ---------------------------------------------------------------------------

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
    use std::cell::RefCell;
    use std::rc::Rc;

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
            let is_request_fonts = serde_json::from_str::<serde_json::Value>(body)
                .ok()
                .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(str::to_owned))
                .as_deref()
                == Some("request_fonts");
            if is_request_fonts {
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

    // Inject platform identifier before keybinding defaults so JS display is correct.
    inject_platform(&webview);

    // Inject keybinding defaults so JS can implement reset-to-default.
    inject_keybinding_defaults(&webview);

    // Inject theme color map for the theme picker.
    inject_theme_colors(&webview);

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

    // Wrap on_close in an Rc<RefCell<Option<...>>> so it can be shared
    // between the delete-event handler and the SIGTERM/SIGINT signal handlers.
    // Each handler calls take() to fire the callback exactly once.
    let on_close = Rc::new(RefCell::new(Some(on_close)));

    let window_for_sigterm = window.clone();
    let on_close_for_sigterm = Rc::clone(&on_close);
    gtk::glib::unix_signal_add_local(libc::SIGTERM, move || {
        let (x, y) = window_for_sigterm.position();
        let (width, height) = window_for_sigterm.size();
        if let Some(cb) = on_close_for_sigterm.borrow_mut().take() {
            cb(SettingsWindowGeometry { x, y, width, height });
        }
        gtk::main_quit();
        gtk::glib::ControlFlow::Break
    });

    let window_for_sigint = window.clone();
    let on_close_for_sigint = Rc::clone(&on_close);
    gtk::glib::unix_signal_add_local(libc::SIGINT, move || {
        let (x, y) = window_for_sigint.position();
        let (width, height) = window_for_sigint.size();
        if let Some(cb) = on_close_for_sigint.borrow_mut().take() {
            cb(SettingsWindowGeometry { x, y, width, height });
        }
        gtk::main_quit();
        gtk::glib::ControlFlow::Break
    });

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

// ---------------------------------------------------------------------------
// macOS: tao + wry settings window
// ---------------------------------------------------------------------------

/// Custom event for the tao event loop.
#[cfg(not(target_os = "linux"))]
enum TaoUserEvent {
    /// Another instance sent a "focus" command via the singleton socket.
    FocusWindow,
    /// A termination signal (SIGTERM/SIGINT) was received.
    Terminate,
}

/// Load configuration and serialise it to JSON for webview injection.
#[cfg(not(target_os = "linux"))]
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

/// Build the tao window with optional saved geometry.
#[cfg(not(target_os = "linux"))]
fn build_tao_window(
    event_loop: &tao::event_loop::EventLoop<TaoUserEvent>,
    geometry: Option<SettingsWindowGeometry>,
) -> Result<tao::window::Window, String> {
    use tao::dpi::{LogicalPosition, LogicalSize};

    let mut builder = tao::window::WindowBuilder::new().with_title("Scribe Settings");

    if let Some(geom) = geometry {
        builder = builder
            .with_inner_size(LogicalSize::new(f64::from(geom.width), f64::from(geom.height)))
            .with_position(LogicalPosition::new(f64::from(geom.x), f64::from(geom.y)));
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
            match singleton::read_command(&stream).as_deref() {
                Some("focus") => drop(proxy.send_event(TaoUserEvent::FocusWindow)),
                Some("quit") => drop(proxy.send_event(TaoUserEvent::Terminate)),
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

/// Run the settings window on macOS using tao + wry (blocking until closed).
///
/// Uses `tao::EventLoop` for windowing and `wry::WebViewBuilder::build()`
/// with the tao window (no GTK dependency).
#[cfg(not(target_os = "linux"))]
#[allow(clippy::too_many_lines, reason = "tao window setup + webview + event loop in one function")]
pub fn run_settings_window(
    geometry: Option<SettingsWindowGeometry>,
    on_change: impl Fn(String) + 'static,
    on_close: impl FnOnce(SettingsWindowGeometry) + 'static,
    listener: std::os::unix::net::UnixListener,
    _socket_path: std::path::PathBuf,
) -> Result<(), String> {
    use std::cell::RefCell;
    use std::rc::Rc;

    use tao::event::{Event, WindowEvent};
    use tao::event_loop::ControlFlow;
    use tao::platform::run_return::EventLoopExtRunReturn;

    let config_json = load_config_json();
    let html = build_html()?;

    let mut event_loop =
        tao::event_loop::EventLoopBuilder::<TaoUserEvent>::with_user_event().build();
    let window = build_tao_window(&event_loop, geometry)?;

    // Spawn singleton listener and signal handlers on background threads.
    spawn_singleton_listener(listener, event_loop.create_proxy());
    register_signal_handlers(event_loop.create_proxy());

    // Shared webview ref for IPC font-refresh requests.
    let webview_ref: Rc<RefCell<Option<wry::WebView>>> = Rc::new(RefCell::new(None));
    let webview_for_ipc = Rc::clone(&webview_ref);

    let webview = wry::WebViewBuilder::new()
        .with_html(&html)
        .with_ipc_handler(move |request| {
            let body = request.body();
            let is_request_fonts = serde_json::from_str::<serde_json::Value>(body)
                .ok()
                .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(str::to_owned))
                .as_deref()
                == Some("request_fonts");
            if is_request_fonts {
                if let Some(wv) = webview_for_ipc.borrow().as_ref() {
                    inject_font_list(wv);
                }
                return;
            }
            on_change(body.clone());
        })
        .build(&window)
        .map_err(|e| format!("failed to create webview: {e}"))?;

    // Inject config, keybinding defaults, and font list.
    let init_script =
        format!("if (typeof loadConfig === 'function') {{ loadConfig({config_json}); }}");
    if let Err(e) = webview.evaluate_script(&init_script) {
        tracing::warn!("failed to inject config into settings webview: {e}");
    }
    inject_platform(&webview);
    inject_keybinding_defaults(&webview);
    inject_theme_colors(&webview);
    inject_font_list(&webview);

    *webview_ref.borrow_mut() = Some(webview);

    let on_close = RefCell::new(Some(on_close));
    let target_window_id = window.id();

    event_loop.run_return(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::UserEvent(TaoUserEvent::FocusWindow) => window.set_focus(),
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. }
            | Event::UserEvent(TaoUserEvent::Terminate) => {
                fire_on_close(&window, &on_close);
                *control_flow = ControlFlow::Exit;
            }
            Event::WindowEvent { event: WindowEvent::Destroyed, window_id: id, .. }
                if id == target_window_id =>
            {
                *control_flow = ControlFlow::Exit;
            }
            _ => {}
        }
    });

    Ok(())
}

/// Fire the `on_close` callback exactly once, capturing current geometry.
#[cfg(not(target_os = "linux"))]
fn fire_on_close(
    window: &tao::window::Window,
    on_close: &std::cell::RefCell<Option<impl FnOnce(SettingsWindowGeometry)>>,
) {
    if let Some(cb) = on_close.borrow_mut().take() {
        cb(capture_geometry(window));
    }
}

/// Handle one incoming connection on the singleton socket.
///
/// Accepts a connection, verifies peer UID, reads the command, and
/// presents the window if the command is `"focus"`.
#[cfg(target_os = "linux")]
fn handle_singleton_connection(listener: &std::os::unix::net::UnixListener, window: &gtk::Window) {
    use gtk::prelude::GtkWindowExt;

    let Ok((stream, _)) = listener.accept() else {
        return;
    };
    if !singleton::verify_peer_uid(&stream) {
        return;
    }
    match singleton::read_command(&stream).as_deref() {
        Some("focus") => window.present(),
        Some("quit") => gtk::main_quit(),
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

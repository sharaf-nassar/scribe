#![allow(unsafe_code, reason = "wry webview FFI bindings require unsafe")]

pub mod colors;
pub mod handler;
pub mod messages;
pub mod repository;
pub mod server_client;
pub mod singleton;
pub mod state;

use rust_embed::Embed;

/// Embedded web assets (HTML, CSS, JS) for the driver UI.
#[derive(Embed)]
#[folder = "src/assets/"]
struct Assets;

/// Saved driver window geometry, returned on close.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct DriverWindowGeometry {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// Build a self-contained HTML document by inlining the CSS and JS assets.
///
/// The resulting HTML can be loaded directly via `wry::WebViewBuilder::with_html`.
pub fn build_html() -> Result<String, String> {
    let html_bytes = Assets::get("driver.html")
        .ok_or_else(|| String::from("embedded asset driver.html not found"))?;
    let css_bytes = Assets::get("driver.css")
        .ok_or_else(|| String::from("embedded asset driver.css not found"))?;
    let js_bytes = Assets::get("driver.js")
        .ok_or_else(|| String::from("embedded asset driver.js not found"))?;

    let html = std::str::from_utf8(&html_bytes.data)
        .map_err(|e| format!("driver.html is not valid UTF-8: {e}"))?;
    let css = std::str::from_utf8(&css_bytes.data)
        .map_err(|e| format!("driver.css is not valid UTF-8: {e}"))?;
    let js = std::str::from_utf8(&js_bytes.data)
        .map_err(|e| format!("driver.js is not valid UTF-8: {e}"))?;

    // Replace the external CSS link with an inline <style> block.
    let html = html.replace(
        r#"<link rel="stylesheet" href="driver.css">"#,
        &format!("<style>\n{css}\n</style>"),
    );

    // Replace the external JS script tag with an inline <script> block.
    let html =
        html.replace(r#"<script src="driver.js"></script>"#, &format!("<script>\n{js}\n</script>"));

    Ok(html)
}

/// Send an IPC response JSON string to the webview via `receiveDriverMessage`.
fn send_ipc_response(
    webview_ref: &std::rc::Rc<std::cell::RefCell<Option<wry::WebView>>>,
    resp: &str,
) {
    let borrow = webview_ref.borrow();
    let Some(wv) = borrow.as_ref() else { return };
    let script =
        format!("if(typeof receiveDriverMessage==='function')receiveDriverMessage({resp})");
    if let Err(e) = wv.evaluate_script(&script) {
        tracing::warn!("failed to send driver IPC response to webview: {e}");
    }
}

/// Inject the initial task state JSON into the webview.
pub fn inject_initial_state(webview: &wry::WebView, state_json: &str) {
    let script =
        format!("if (typeof loadDriverState === 'function') {{ loadDriverState({state_json}); }}");
    if let Err(e) = webview.evaluate_script(&script) {
        tracing::warn!("failed to inject driver state into webview: {e}");
    }
}

// ---------------------------------------------------------------------------
// Linux: GTK-based driver window
// ---------------------------------------------------------------------------

/// Run the driver window (blocking until closed).
///
/// On Linux, initialises GTK, creates the window, registers the singleton
/// socket fd watcher, installs signal handlers, and enters `gtk::main()`.
///
/// `on_ipc` is called for each IPC message from the webview.
/// `on_close` is called with the final geometry when the window closes.
#[cfg(target_os = "linux")]
#[allow(
    clippy::too_many_lines,
    reason = "GTK window setup + webview + socket watcher in one function"
)]
#[allow(
    clippy::too_many_arguments,
    reason = "all parameters are needed: geometry, initial state, two callbacks, listener, socket path"
)]
pub fn run_driver_window(
    geometry: Option<DriverWindowGeometry>,
    initial_state_json: &str,
    on_ipc: impl Fn(String) -> String + 'static,
    on_close: impl FnOnce(DriverWindowGeometry) + 'static,
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

    let html = build_html()?;

    let window = gtk::Window::new(gtk::WindowType::Toplevel);
    window.set_title("Scribe Driver");

    // Restore saved size, or use defaults.
    if let Some(geom) = geometry {
        window.set_default_size(geom.width, geom.height);
    } else {
        window.set_default_size(1000, 700);
        window.set_position(gtk::WindowPosition::Center);
    }

    // Create a GTK Box to hold the webview.
    let container = gtk::Box::new(gtk::Orientation::Vertical, 0);
    window.add(&container);
    window.show_all();

    // Restore saved position after the window is visible.
    if let Some(geom) = geometry {
        if !is_wayland_backend() {
            window.move_(geom.x, geom.y);
        }
    }

    // Shared webview reference so the IPC handler can call evaluate_script.
    let webview_ref: Rc<RefCell<Option<wry::WebView>>> = Rc::new(RefCell::new(None));
    let webview_ref_for_ipc = Rc::clone(&webview_ref);

    let webview = wry::WebViewBuilder::new()
        .with_html(&html)
        .with_ipc_handler(move |request| {
            let resp = on_ipc(request.body().clone());
            if !resp.is_empty() {
                send_ipc_response(&webview_ref_for_ipc, &resp);
            }
        })
        .build_gtk(&container)
        .map_err(|e| format!("failed to create webview: {e}"))?;

    // Inject initial task state.
    inject_initial_state(&webview, initial_state_json);

    // Store webview in the shared ref.
    *webview_ref.borrow_mut() = Some(webview);

    // Watch the singleton socket for incoming focus commands.
    let fd = std::os::unix::io::AsRawFd::as_raw_fd(&listener);
    let window_for_focus = window.clone();
    gtk::glib::unix_fd_add_local(fd, gtk::glib::IOCondition::IN, move |_, _| {
        handle_singleton_connection(&listener, &window_for_focus);
        gtk::glib::ControlFlow::Continue
    });

    let on_close = Rc::new(RefCell::new(Some(on_close)));

    let window_for_sigterm = window.clone();
    let on_close_for_sigterm = Rc::clone(&on_close);
    gtk::glib::unix_signal_add_local(libc::SIGTERM, move || {
        let (x, y) = window_for_sigterm.position();
        let (width, height) = window_for_sigterm.size();
        if let Some(cb) = on_close_for_sigterm.borrow_mut().take() {
            cb(DriverWindowGeometry { x, y, width, height });
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
            cb(DriverWindowGeometry { x, y, width, height });
        }
        gtk::main_quit();
        gtk::glib::ControlFlow::Break
    });

    window.connect_delete_event(move |win, _| {
        let (x, y) = win.position();
        let (width, height) = win.size();
        if let Some(cb) = on_close.borrow_mut().take() {
            cb(DriverWindowGeometry { x, y, width, height });
        }
        gtk::main_quit();
        gtk::glib::Propagation::Proceed
    });

    gtk::main();

    Ok(())
}

// ---------------------------------------------------------------------------
// macOS: tao + wry driver window
// ---------------------------------------------------------------------------

/// Custom event for the tao event loop.
#[cfg(not(target_os = "linux"))]
enum TaoUserEvent {
    /// Another instance sent a "focus" command via the singleton socket.
    FocusWindow,
    /// A termination signal (SIGTERM/SIGINT) was received.
    Terminate,
}

/// Build the tao window with optional saved geometry.
#[cfg(not(target_os = "linux"))]
fn build_tao_window(
    event_loop: &tao::event_loop::EventLoop<TaoUserEvent>,
    geometry: Option<DriverWindowGeometry>,
) -> Result<tao::window::Window, String> {
    use tao::dpi::{LogicalPosition, LogicalSize};

    let mut builder = tao::window::WindowBuilder::new().with_title("Scribe Driver");

    if let Some(geom) = geometry {
        builder = builder
            .with_inner_size(LogicalSize::new(f64::from(geom.width), f64::from(geom.height)))
            .with_position(LogicalPosition::new(f64::from(geom.x), f64::from(geom.y)));
    } else {
        builder = builder.with_inner_size(LogicalSize::new(1000.0, 700.0));
    }

    builder.build(event_loop).map_err(|e| format!("failed to create window: {e}"))
}

/// Spawn a background thread to accept singleton socket connections.
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

/// Capture the current window geometry as a `DriverWindowGeometry`.
#[cfg(not(target_os = "linux"))]
fn capture_geometry(window: &tao::window::Window) -> DriverWindowGeometry {
    let pos = window.outer_position().unwrap_or_default();
    let size = window.inner_size();
    DriverWindowGeometry {
        x: pos.x,
        y: pos.y,
        width: size.width.cast_signed(),
        height: size.height.cast_signed(),
    }
}

/// Fire the `on_close` callback exactly once, capturing current geometry.
#[cfg(not(target_os = "linux"))]
fn fire_on_close(
    window: &tao::window::Window,
    on_close: &std::cell::RefCell<Option<impl FnOnce(DriverWindowGeometry)>>,
) {
    if let Some(cb) = on_close.borrow_mut().take() {
        cb(capture_geometry(window));
    }
}

/// Run the driver window on macOS using tao + wry (blocking until closed).
#[cfg(not(target_os = "linux"))]
#[allow(clippy::too_many_lines, reason = "tao window setup + webview + event loop in one function")]
#[allow(
    clippy::too_many_arguments,
    reason = "all parameters are needed: geometry, initial state, two callbacks, listener, socket path"
)]
pub fn run_driver_window(
    geometry: Option<DriverWindowGeometry>,
    initial_state_json: &str,
    on_ipc: impl Fn(String) -> String + 'static,
    on_close: impl FnOnce(DriverWindowGeometry) + 'static,
    listener: std::os::unix::net::UnixListener,
    _socket_path: std::path::PathBuf,
) -> Result<(), String> {
    use std::cell::RefCell;
    use std::rc::Rc;

    use tao::event::{Event, WindowEvent};
    use tao::event_loop::ControlFlow;
    use tao::platform::run_return::EventLoopExtRunReturn;

    let html = build_html()?;

    let mut event_loop =
        tao::event_loop::EventLoopBuilder::<TaoUserEvent>::with_user_event().build();
    let window = build_tao_window(&event_loop, geometry)?;

    // Spawn singleton listener and signal handlers on background threads.
    spawn_singleton_listener(listener, event_loop.create_proxy());
    register_signal_handlers(event_loop.create_proxy());

    // Shared webview reference so the IPC handler can call evaluate_script.
    let webview_ref: Rc<RefCell<Option<wry::WebView>>> = Rc::new(RefCell::new(None));
    let webview_ref_for_ipc = Rc::clone(&webview_ref);

    let webview = wry::WebViewBuilder::new()
        .with_html(&html)
        .with_ipc_handler(move |request| {
            let resp = on_ipc(request.body().clone());
            if !resp.is_empty() {
                if let Some(wv) = webview_ref_for_ipc.borrow().as_ref() {
                    let script = format!(
                        "if(typeof receiveDriverMessage==='function')receiveDriverMessage({resp})"
                    );
                    if let Err(e) = wv.evaluate_script(&script) {
                        tracing::warn!("failed to send driver IPC response to webview: {e}");
                    }
                }
            }
        })
        .build(&window)
        .map_err(|e| format!("failed to create webview: {e}"))?;

    inject_initial_state(&webview, initial_state_json);

    // Store webview in the shared ref.
    *webview_ref.borrow_mut() = Some(webview);

    let on_close = RefCell::new(Some(on_close));
    let target_window_id = window.id();

    event_loop.run_return(move |event, _, control_flow| {
        // Keep webview_ref alive for the duration of the event loop.
        let _keep_webview = &webview_ref;
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

/// Handle one incoming connection on the singleton socket (Linux only).
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
#[cfg(target_os = "linux")]
fn is_wayland_backend() -> bool {
    match std::env::var("GDK_BACKEND").ok().as_deref() {
        Some("x11") => false,
        Some("wayland") => true,
        _ => std::env::var_os("WAYLAND_DISPLAY").is_some(),
    }
}

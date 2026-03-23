#![allow(unsafe_code, reason = "wry webview FFI bindings require unsafe")]

use rust_embed::Embed;

/// Embedded web assets (HTML, CSS, JS) for the settings UI.
#[derive(Embed)]
#[folder = "src/assets/"]
struct Assets;

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

/// Open the settings window.
///
/// `config_json` is the current config serialized as JSON.
/// `on_change` is called with JSON messages when the user changes a setting.
///
/// The window opens on a separate thread so the caller's event loop is not blocked.
///
/// Returns `Ok(())` if the window thread was spawned, or an error if assets
/// could not be loaded.
pub fn open_settings_window(
    config_json: String,
    on_change: impl Fn(String) + Send + 'static,
) -> Result<(), String> {
    let html = build_html()?;

    std::thread::spawn(move || {
        if let Err(e) = run_settings_window(&html, &config_json, on_change) {
            tracing::warn!("settings window failed: {e}");
        }
    });

    Ok(())
}

/// Run the settings window on the current thread (blocking).
///
/// Uses GTK + wry `WebViewBuilderExtUnix::build_gtk` for Wayland + X11 support.
fn run_settings_window(
    html: &str,
    config_json: &str,
    on_change: impl Fn(String) + Send + 'static,
) -> Result<(), String> {
    use gtk::prelude::*;
    use wry::WebViewBuilderExtUnix;

    gtk::init().map_err(|e| format!("GTK init failed (is WebKitGTK installed?): {e}"))?;

    let window = gtk::Window::new(gtk::WindowType::Toplevel);
    window.set_title("Scribe Settings");
    window.set_default_size(880, 680);
    window.set_position(gtk::WindowPosition::Center);

    // Create a GTK Box to hold the webview.
    let container = gtk::Box::new(gtk::Orientation::Vertical, 0);
    window.add(&container);
    window.show_all();

    let webview = wry::WebViewBuilder::new()
        .with_html(html)
        .with_ipc_handler(move |request| {
            let body = request.body();
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

    // Close the GTK main loop when the window is closed.
    window.connect_delete_event(|_, _| {
        gtk::main_quit();
        gtk::glib::Propagation::Proceed
    });

    // Keep the webview alive for the duration of the GTK main loop.
    // Without this, the webview would be dropped and the window would be blank.
    let _webview = webview;

    gtk::main();

    Ok(())
}

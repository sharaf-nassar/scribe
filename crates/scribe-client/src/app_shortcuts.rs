//! Native application shortcuts that sit above terminal key translation.
//!
//! These actions are intentionally separate from [`crate::keybindings`]:
//! `cmd+q` and `cmd+w` belong to the macOS application/window lifecycle, not
//! to a terminal pane, and must remain available while an overlay owns focus.

use gpui::App;

gpui::actions!(scribe, [Quit, CloseWindow]);

/// Install the standard macOS application shortcuts and native menu entries.
// @lat: [[client#Client#Input#Key Translation Priority]]
#[cfg(target_os = "macos")]
pub fn register(cx: &mut App) {
    use gpui::{KeyBinding, Menu, MenuItem, SystemMenuType};

    cx.bind_keys([
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-w", CloseWindow, None),
    ]);
    cx.set_menus([
        Menu::new("Scribe").items([
            MenuItem::os_submenu("Services", SystemMenuType::Services),
            MenuItem::separator(),
            MenuItem::action("Quit Scribe", Quit),
        ]),
        Menu::new("File").items([MenuItem::action("Close Window", CloseWindow)]),
    ]);
}

/// Other platforms retain their existing configurable keybinding defaults.
#[cfg(not(target_os = "macos"))]
pub fn register(_cx: &mut App) {}

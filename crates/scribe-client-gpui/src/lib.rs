//! Library surface of the GPUI Scribe client rebuild.
//!
//! Holds the display-independent building blocks the GPUI paint path consumes
//! — the xterm-256 [`palette`], terminal cell [`color`] semantics, and the
//! procedural [`box_drawing`] rasterizer — alongside the terminal [`input`]
//! byte encoder. Each of those modules is ported byte-for-byte from the legacy
//! client so terminal output stays identical across the cutover. The
//! [`mouse_reporting`] byte encoder (X10 / SGR-1006, modes 1000/1002/1003) and
//! the [`mouse_state`] click-count / selection-mode classifier are ported the
//! same way.
//!
//! It also hosts the ported pane/workspace split trees and their GPUI entity
//! wrappers. The pure split-tree logic lives in [`layout`] and
//! [`workspace_layout`]; the [`pane_tree`] and [`workspace_tree`] modules wrap
//! that logic in `gpui::Entity` models that emit change events (the workspace
//! model emits the `ReportWorkspaceTree` payload) on every mutation so the
//! chrome and IPC layers can react.
//!
//! Alongside those it hosts the ported terminal-interaction pieces: [`preedit`]
//! (IME composition state, overlay geometry, and a `gpui::EntityInputHandler`),
//! [`paste`] (bracketed-paste detection gating the confirmation dialog), and
//! [`bell`] (routing the terminal bell to a tab attention badge plus the system
//! bell). The pure classifiers are unit-tested and the entity gates are covered
//! by `#[gpui::test]`; IME is verified by the manual parity procedure.
//!
//! The OS-integration ports live here too: [`clipboard`] (arboard handle, the
//! OSC 52 read/write bridge, Linux primary selection) with its
//! [`clipboard_cleanup`] AI copy transforms, and [`notification_dispatcher`]
//! (the zbus `replaces_id` coalescing state machine and freedesktop timeout
//! mapping, with click-to-focus emitted on a runtime-agnostic channel).
//!
//! The window chrome starts with the [`status_bar`]: a pure segment model
//! (connection dot, command/env glyphs, workspace/CWD/git/host labels, tmux and
//! session badges, clock, the centred update CTA, and the 013/015 remote-control
//! and share-presence surfaces) lowered onto a GPUI flex row, fed by the
//! [`sys_stats`] CPU/memory/network/GPU sampler that drives its sparklines. The
//! CTA's own inputs come from [`update`], which holds the server's latest
//! `UpdateAvailable` / `UpdateProgress` broadcast and resolves the confirmation
//! modal a click on the CTA opens.
//!
//! The binary shell consumes a growing slice of this surface directly: the
//! [`keybindings`] parser now drives the live key path, [`tab_session`]
//! holds the ordered tab strip those tab shortcuts mutate, and
//! [`window_lifecycle`] is the close / quit / window-list / focus-report state
//! the IPC reader and the GPUI view share across threads. The feature-014 LAN
//! surface follows the same split: [`lan`] is the shared chrome the reader
//! folds every LAN answer into, [`lan_approval`] is the owning-side prompt it
//! parks for the window to raise, and [`lan_dial`] is the connecting side's
//! identity fetch, mutual-TLS dial, and approval-gate preamble. The remaining
//! consumers (pane splits, scrollback navigation, zoom) land in later beads of
//! the `gpui-client-rebuild` epic.

pub mod ai_indicator;
pub mod animation;
pub mod bell;
pub mod box_drawing;
pub mod chrome_metadata;
pub mod clipboard;
pub mod clipboard_cleanup;
pub mod color;
pub mod command_palette;
pub mod config;
pub mod context_menu;
pub mod dialog;
pub mod divider;
pub mod drag_drop;
pub mod focus_border;
pub mod fonts;
pub mod input;
pub mod keybindings;
pub mod lan;
pub mod lan_approval;
pub mod lan_dial;
pub mod layout;
pub mod lost_control;
pub mod mouse_reporting;
pub mod mouse_state;
pub mod notification_dispatcher;
pub mod opacity;
pub mod palette;
pub mod pane_tree;
pub mod paste;
pub mod preedit;
pub mod prompt_bar;
pub mod remote;
pub mod remote_handshake;
pub mod restore_replay;
pub mod restore_state;
pub mod scrollbar;
pub mod search;
pub mod selection;
pub mod server_lifecycle;
pub mod settings;
pub mod share;
pub mod share_join;
pub mod smart_selection;
pub mod split_scroll;
pub mod status_bar;
pub mod sys_stats;
pub mod tab_bar;
pub mod tab_session;
pub mod titlebar;
pub mod tooltip;
pub mod update;
pub mod url_detect;
pub mod vi_mode;
pub mod window_chrome;
pub mod window_lifecycle;
pub mod window_state;
pub mod workspace_layout;
pub mod workspace_notes;
pub mod workspace_notes_modal;
pub mod workspace_notes_preview;
pub mod workspace_tree;
#[cfg(target_os = "linux")]
pub mod x11_focus;
pub mod zoom;

/// Assert two linear RGBA colours are bit-for-bit identical.
///
/// The rendering-parity tests require byte-exact matches against the legacy
/// renderer, so channels are compared by their raw IEEE-754 bit patterns
/// (`f32::to_bits`) rather than with `==`, which also keeps the strict
/// `clippy::float_cmp` lint satisfied without a suppression.
#[cfg(test)]
pub(crate) fn assert_rgba_eq(actual: [f32; 4], expected: [f32; 4]) {
    for (i, (a, e)) in actual.iter().zip(expected).enumerate() {
        assert_eq!(
            a.to_bits(),
            e.to_bits(),
            "channel {i} differs: {a} (0x{:08x}) != {e} (0x{:08x})",
            a.to_bits(),
            e.to_bits(),
        );
    }
}

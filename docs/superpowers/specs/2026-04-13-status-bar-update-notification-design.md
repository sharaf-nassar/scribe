# Status Bar Update Notification Design

This spec moves the update call-to-action from workspace-local chrome to window-level chrome so one update state is shown once per window.

## Problem

The update availability state is window-global, but the prior in-window affordance appeared in workspace tab bars. In multi-workspace windows that duplicated the same notification across workspaces and implied the state was workspace-specific.

The native window title can display update state, but it is not an application-controlled clickable surface in the current decorated-window setup. The clickable affordance needs to live inside Scribe-rendered chrome.

## Goals

- Show update availability once per window.
- Make the clickable update affordance easy to find.
- Keep the existing update dialog and updater control flow.
- Reuse the same visual slot for update progress after the user confirms.

## Non-Goals

- Replacing native OS title bar behavior with custom window decorations.
- Adding new update actions or changing updater protocol messages.
- Introducing new automated tests unless requested separately.

## Chosen Approach

Render a dedicated update segment in the center of the window-level bottom status bar.

When an update is available, the center segment shows `↑ Update to v{version}` and is clickable. Clicking it opens the existing in-app update dialog. After confirmation, the same center segment becomes non-clickable progress text sourced from existing `UpdateProgressState` values.

The native window title continues to announce update availability and appends guidance text: ` - click below to update`.

## UI Behavior

### Available State

When `update_available` is present, Scribe renders a single centered status-bar update control. The title becomes `{window_title} - v{version} available - click below to update`.

### Busy State

When `update_progress` is present and no new availability CTA is shown, the center status-bar slot displays one of:

- `Downloading...`
- `Verifying...`
- `Installing...`
- `Updated!`
- `Updated! Restart required`
- `Update failed`

The busy-state slot is display-only and does not open the dialog.

### Idle State

When no update is available and no update progress is active, the center slot is absent and the status bar falls back to its existing left/right content only.

## Rendering and Input Changes

### Status Bar Renderer

`crates/scribe-client/src/status_bar.rs` gains a dedicated center segment and hit target. The renderer remains window-level and separate from workspace tab bars.

The center segment is not part of the left-side context group or the right-side stats/actions group. It is positioned independently so the update CTA remains visually distinct and does not inherit per-workspace semantics.

### App State and Click Routing

`crates/scribe-client/src/main.rs` stores a status-bar update hit rect alongside the existing gear/equalize hit rects. Mouse handling checks that hit target and routes it to the existing `open_update_dialog()` path.

The tab-bar update affordance and its hit target are removed from normal rendering and click routing so only one in-window update CTA remains.

### Title Text

`App::update_window_title()` remains the single place that maps update availability to native title text. It is updated to append the user guidance suffix only while `update_available` is present.

## Data Flow

The existing flow remains unchanged:

1. The server broadcasts `UpdateAvailable { version, release_url }`.
2. The client stores `update_available`.
3. The client updates the native title and renders the centered status-bar CTA.
4. User click opens the existing update dialog.
5. Confirm sends `TriggerUpdate`.
6. Server broadcasts `UpdateProgress`.
7. Client clears the CTA, shows progress in the center slot, and continues using the existing updater lifecycle.

## Risks and Mitigations

### Narrow Window Layout

The centered segment could contend with left/right status content in narrow widths. The implementation keeps the update text short and preserves a single centered slot instead of allowing repeated copies or fallback workspace-local render paths.

### Mixed Availability and Progress State

The renderer must continue to prefer availability CTA over progress only when that is the intended current state. Existing client state handling already clears `update_available` when the user confirms or dismisses, which keeps the center slot unambiguous.

## Verification

- `cargo check -p scribe-client`
- `lat check`
- Code inspection to confirm:
  - available state renders one centered clickable status-bar CTA
  - click routing opens the existing update dialog
  - progress state renders in the same center slot and is not clickable
  - native title includes ` - click below to update` only while an update is available

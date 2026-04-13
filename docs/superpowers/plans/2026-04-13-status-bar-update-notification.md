# Status Bar Update Notification Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move the update CTA to a single centered slot in the window-level status bar, keep progress in that same slot, and update the native title to say ` - click below to update`.

**Architecture:** The window-level status bar in `crates/scribe-client/src/status_bar.rs` will own the update CTA/progress display and publish a dedicated hit target. `crates/scribe-client/src/main.rs` will pass update state into that renderer, route clicks to the existing update dialog, remove the old tab-bar update hit path, and update the native title text. `lat.md/client.md` will document the new window-level status-bar behavior.

**Tech Stack:** Rust, winit, wgpu-backed chrome rendering, lat.md docs

---

## File Map

- Modify: `crates/scribe-client/src/status_bar.rs`
  - Add window-level update data fields, centered status-bar rendering, and a dedicated update hit rect.
- Modify: `crates/scribe-client/src/main.rs`
  - Store the status-bar update hit rect, pass update state into the status bar, route clicks, remove the tab-bar update click path, and change native title copy.
- Modify: `crates/scribe-client/src/tab_bar.rs`
  - Remove the unused tab-bar update button/progress API and rendering helpers so the update affordance only exists once in-window.
- Modify: `lat.md/client.md`
  - Document that update availability appears in the native title and centered bottom status bar instead of per-workspace tab bars.

### Task 1: Add A Centered Window-Level Update Segment

**Files:**
- Modify: `crates/scribe-client/src/status_bar.rs`

- [ ] **Step 1: Extend status-bar input and output types for update UI**

Add update state to the status-bar render input and add a dedicated hit rect to the render output.

```rust
pub struct StatusBarHitTargets {
    pub update_rect: Option<Rect>,
    pub equalize_rect: Option<Rect>,
    pub gear_rect: Option<Rect>,
    pub tooltip_targets: Vec<TooltipAnchor>,
}

pub struct StatusBarData<'a> {
    pub connected: bool,
    pub show_equalize: bool,
    pub workspace_name: Option<&'a str>,
    pub cwd: Option<&'a Path>,
    pub git_branch: Option<&'a str>,
    pub session_count: usize,
    pub host_label: &'a str,
    pub tmux_label: Option<&'a str>,
    pub time: &'a str,
    pub update_available: Option<&'a str>,
    pub update_progress: Option<&'a UpdateProgressState>,
    pub sys_stats: Option<&'a SystemStats>,
    pub stats_config: Option<&'a scribe_common::config::StatusBarStatsConfig>,
}
```

- [ ] **Step 2: Add a helper that resolves the centered label and whether it is clickable**

Keep the decision in one place so availability and progress share the same center slot.

```rust
fn centered_update_label(data: &StatusBarData<'_>) -> Option<(String, bool)> {
    if let Some(version) = data.update_available {
        return Some((format!("\u{2191} Update to v{version}"), true));
    }

    let label = match data.update_progress {
        Some(UpdateProgressState::Downloading) => Some(String::from("Downloading...")),
        Some(UpdateProgressState::Verifying) => Some(String::from("Verifying...")),
        Some(UpdateProgressState::Installing) => Some(String::from("Installing...")),
        Some(UpdateProgressState::Completed { .. }) => Some(String::from("Updated!")),
        Some(UpdateProgressState::CompletedRestartRequired { .. }) => {
            Some(String::from("Updated! Restart required"))
        }
        Some(UpdateProgressState::Failed { .. }) => Some(String::from("Update failed")),
        None => None,
    }?;

    Some((label, false))
}
```

- [ ] **Step 3: Render the centered segment independently from left/right status content**

Center the label in the window-level bar, emit the glyphs with accent text for the clickable CTA and normal text for progress, and record `update_rect` only for the clickable state.

```rust
fn render_center_update(
    w: &mut BarWriter<'_>,
    colors: &StatusBarColors,
    data: &StatusBarData<'_>,
    resolve_glyph: &mut dyn FnMut(char) -> ([f32; 2], [f32; 2]),
    tooltips: &mut Vec<TooltipAnchor>,
) -> Option<Rect> {
    let Some((label, clickable)) = centered_update_label(data) else { return None };
    let label_cols = label.chars().count();
    let start_col = w.max_cols.saturating_sub(label_cols) / 2;
    let fg = if clickable { colors.accent } else { colors.text };

    let mut writer = BarWriter { col: start_col, ..*w };
    writer.put_str(&label, fg, colors.bg, resolve_glyph);
    let rect = writer.col_rect(start_col);

    if clickable {
        tooltips.push(TooltipAnchor { text: String::from("Update Scribe"), rect });
        Some(rect)
    } else {
        None
    }
}
```

- [ ] **Step 4: Return the new hit target from `build_status_bar`**

Keep the existing left and right rendering order, but capture the centered update rect in the returned `StatusBarHitTargets`.

Run: `cargo check -p scribe-client`
Expected: the crate still compiles after adding the new status-bar fields and helper signatures

### Task 2: Wire The App To The New Center Slot

**Files:**
- Modify: `crates/scribe-client/src/main.rs`

- [ ] **Step 1: Replace tab-bar update hit storage with a status-bar update rect**

The app keeps one update hit target at window scope instead of a per-workspace vector.

```rust
// Update state
update_available: Option<(String, String)>,
update_progress: Option<UpdateProgressState>,
update_dialog: Option<update_dialog::UpdateDialog>,

// Status bar hit targets
status_bar_update_rect: Option<layout::Rect>,
status_bar_gear_rect: Option<layout::Rect>,
status_bar_equalize_rect: Option<layout::Rect>,
```

- [ ] **Step 2: Pass update state into `StatusBarData` and store the returned hit rect**

Feed the centered status-bar CTA from the existing app state and save the returned `update_rect`.

```rust
let sb_data = status_bar::StatusBarData {
    connected: self.server_connected,
    show_equalize: multi_workspace,
    workspace_name: focused_ws_name.as_deref(),
    cwd: focused_pane_cwd.as_deref(),
    git_branch: focused_pane_git.as_deref(),
    session_count,
    host_label,
    tmux_label,
    time: &time_str,
    update_available: self.update_available.as_ref().map(|(v, _)| v.as_str()),
    update_progress: self.update_progress.as_ref(),
    sys_stats: Some(self.sys_stats.stats()),
    stats_config: Some(&self.config.terminal.status_bar_stats),
};

self.status_bar_update_rect = sb_hits.update_rect;
```

- [ ] **Step 3: Route clicks through the new status-bar hit rect and remove the tab-bar update click path**

The clickable update CTA now lives only in the status bar and still opens the existing dialog.

```rust
if let Some(update_rect) = self.status_bar_update_rect {
    if update_rect.contains(x, y) {
        self.open_update_dialog();
        return;
    }
}

// Remove:
// if self.tab_bar_update_targets.iter().any(|(_, rect)| rect.contains(x, y)) { ... }
```

- [ ] **Step 4: Update the native title copy**

Append the guidance suffix only while `update_available` is present.

```rust
fn update_window_title(&self) {
    if let Some(window) = &self.window {
        let window_title = current_identity().window_title_name();
        if let Some((version, _)) = &self.update_available {
            window.set_title(&format!(
                "{window_title} - v{version} available - click below to update"
            ));
        } else {
            window.set_title(window_title);
        }
    }
}
```

- [ ] **Step 5: Verify the client still builds after the app-state wiring changes**

Run: `cargo check -p scribe-client`
Expected: `Finished 'dev' profile` with no compile errors

### Task 3: Remove The Old Tab-Bar Update Path And Sync Docs

**Files:**
- Modify: `crates/scribe-client/src/tab_bar.rs`
- Modify: `lat.md/client.md`

- [ ] **Step 1: Remove tab-bar update-specific fields and helpers**

Delete the now-unused update fields from `TabBarTextParams` and `TabBarHitTargets`, plus the old tab-bar update/progress helpers.

```rust
pub struct TabBarTextParams<'a> {
    pub rect: Rect,
    pub cell_size: (f32, f32),
    pub tabs: &'a [TabData],
    pub badge: Option<(&'a str, Option<[f32; 4]>)>,
    pub show_gear: bool,
    pub show_equalize: bool,
    pub colors: &'a TabBarColors,
    pub resolve_glyph: &'a mut dyn FnMut(char) -> ([f32; 2], [f32; 2]),
    pub tab_bar_height: f32,
    pub indicator_height: f32,
    pub tab_width: u16,
    pub hovered_tab_close: Option<usize>,
    pub hovered_tab: Option<usize>,
    pub tab_offsets: &'a [f32],
    pub dragging_tab: Option<usize>,
    pub drag_cursor_x: f32,
    pub drag_grab_offset: f32,
    pub accent_color: [f32; 4],
}
```

- [ ] **Step 2: Update `lat.md/client.md` to describe the new window-level status-bar CTA**

Keep the update dialog and status bar sections aligned with the implementation.

```md
## Status Bar

The status bar at the bottom of the window shows connection status, workspace info, CWD, git branch, session count, host context, tmux context, time, system stats, and a centered update CTA/progress slot when the updater is active.

### Update Dialog

The update notification appears in the compositor window title and in a centered window-level status-bar CTA rather than in per-workspace tab bars.
```

- [ ] **Step 3: Run the required repository verification**

Run: `cargo check -p scribe-client`
Expected: build succeeds

Run: `lat check`
Expected: `All checks passed`

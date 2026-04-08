# Prompt Bar Floating Card Redesign Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the current blocky prompt bar with a floating card that has a shared bridged dismiss capsule, clearer row hierarchy, and stronger copy/dismiss affordances without changing prompt collection or settings surface area.

**Architecture:** Keep the existing prompt-bar state in `Pane` and `App`, but move prompt-bar geometry into `crates/scribe-client/src/prompt_bar.rs` so rendering, hit-testing, truncation, tooltip sizing, and height calculation all share one layout model. Reuse the existing `PromptBarColors`, prompt lifecycle, and top/bottom placement logic; only the chrome, interaction affordances, and layout math change.

**Tech Stack:** Rust, `scribe-client`, custom GPU `CellInstance` rendering, `lat.md`

---

**Plan note:** Repository guidance says not to add new automated tests unless the user explicitly requests them. This plan uses `cargo fmt`, `cargo check`, and manual UI verification instead of adding new test files.

**Execution note:** Do not restart the Scribe server without explicit user approval. Use an existing running Scribe client/window for manual verification if possible.

## File Map

- Modify: `crates/scribe-client/src/prompt_bar.rs` — shared geometry, floating-card rendering, bridged dismiss capsule, prompt-count badge, truncation width, and hit-testing.
- Modify: `crates/scribe-client/src/pane.rs` — prompt-bar height should call the shared geometry helper instead of duplicating padding math.
- Modify: `crates/scribe-client/src/main.rs` — prompt-bar hover/pressed state plumbing, shared target lookup, click handling, tooltip sizing, and render call integration.
- Modify: `lat.md/client.md` — update the Prompt Bar section to document the floating card, bridged dismiss capsule, rect-based hit-testing, and shared geometry model.

### Task 1: Centralize Prompt Bar Geometry

**Files:**
- Modify: `crates/scribe-client/src/prompt_bar.rs`
- Modify: `crates/scribe-client/src/pane.rs`

- [ ] **Step 1: Add shared floating-card metrics and layout structs to `crates/scribe-client/src/prompt_bar.rs`.**

```rust
pub const CARD_INSET_X: f32 = 10.0;
pub const CARD_INSET_Y: f32 = 6.0;
pub const CARD_RADIUS: f32 = 12.0;
pub const ROW_SIDE_PAD: f32 = 14.0;
pub const ROW_GAP: f32 = 4.0;
pub const ROW_MIN_HEIGHT: f32 = 28.0;
pub const DISMISS_CAPSULE_W: f32 = 24.0;
pub const DISMISS_CAPSULE_H: f32 = 24.0;
pub const DISMISS_CAPSULE_X: f32 = 10.0;
pub const ICON_TEXT_GAP: f32 = 10.0;
pub const COUNT_BADGE_H: f32 = 18.0;
pub const COUNT_BADGE_PAD_X: f32 = 8.0;
pub const COUNT_BADGE_MIN_W: f32 = 30.0;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PromptBarHover {
    FirstRow,
    LatestRow,
    DismissCapsule,
}

#[derive(Clone, Copy)]
pub struct PromptBarLayout {
    pub card_rect: Rect,
    pub first_row_rect: Rect,
    pub latest_row_rect: Option<Rect>,
    pub seam_rect: Option<Rect>,
    pub dismiss_rect: Rect,
    pub count_badge_rect: Option<Rect>,
    pub first_text_width: f32,
    pub latest_text_width: Option<f32>,
}
```

- [ ] **Step 2: Add shared layout helpers in `crates/scribe-client/src/prompt_bar.rs` and use them as the only source of prompt-bar geometry.**

```rust
pub fn prompt_bar_height(prompt_count: u32, cell_height: f32) -> f32 {
    let row_height = (cell_height + 10.0).max(ROW_MIN_HEIGHT);
    let stacked_height = if prompt_count >= 2 {
        row_height * 2.0 + ROW_GAP
    } else {
        row_height
    };
    stacked_height + CARD_INSET_Y * 2.0
}

pub fn compute_prompt_bar_layout(
    pane: &Pane,
    bar_rect: Rect,
    cell_size: (f32, f32),
) -> Option<PromptBarLayout> {
    if pane.prompt_count == 0 {
        return None;
    }

    let (cell_w, cell_h) = cell_size;
    if cell_w <= 0.0 || cell_h <= 0.0 {
        return None;
    }

    let row_height = (cell_h + 10.0).max(ROW_MIN_HEIGHT);
    let card_rect = Rect {
        x: bar_rect.x + CARD_INSET_X,
        y: bar_rect.y + CARD_INSET_Y,
        width: (bar_rect.width - CARD_INSET_X * 2.0).max(1.0),
        height: (bar_rect.height - CARD_INSET_Y * 2.0).max(1.0),
    };
    let first_row_rect = Rect {
        x: card_rect.x,
        y: card_rect.y,
        width: card_rect.width,
        height: row_height,
    };
    let latest_row_rect = (pane.prompt_count >= 2).then_some(Rect {
        x: card_rect.x,
        y: first_row_rect.y + row_height + ROW_GAP,
        width: card_rect.width,
        height: row_height,
    });
    let seam_rect = latest_row_rect.map(|latest| Rect {
        x: card_rect.x + 12.0,
        y: latest.y - ROW_GAP,
        width: card_rect.width - 24.0,
        height: 1.0,
    });
    let dismiss_center_y = latest_row_rect.map_or(
        first_row_rect.y + first_row_rect.height * 0.5,
        |latest| first_row_rect.y + first_row_rect.height + ROW_GAP * 0.5,
    );
    let dismiss_rect = Rect {
        x: card_rect.x + DISMISS_CAPSULE_X,
        y: dismiss_center_y - DISMISS_CAPSULE_H * 0.5,
        width: DISMISS_CAPSULE_W,
        height: DISMISS_CAPSULE_H,
    };
    let count_badge_rect = (pane.prompt_count > 1).then_some(Rect {
        x: card_rect.x + card_rect.width - 44.0,
        y: first_row_rect.y + (first_row_rect.height - COUNT_BADGE_H) * 0.5,
        width: 34.0,
        height: COUNT_BADGE_H,
    });

    let text_left = dismiss_rect.x + dismiss_rect.width + 12.0;
    let badge_reserved = count_badge_rect.map_or(0.0, |rect| rect.width + 8.0);
    let text_width = (card_rect.x + card_rect.width - ROW_SIDE_PAD - badge_reserved - text_left)
        .max(cell_w);

    Some(PromptBarLayout {
        card_rect,
        first_row_rect,
        latest_row_rect,
        seam_rect,
        dismiss_rect,
        count_badge_rect,
        first_text_width: text_width,
        latest_text_width: latest_row_rect.map(|_| text_width),
    })
}
```

- [ ] **Step 3: Update `crates/scribe-client/src/pane.rs` so prompt-bar height uses the shared helper instead of duplicating old strip math.**

```rust
pub fn prompt_bar_height(&self, cell_height: f32, prompt_bar_enabled: bool) -> f32 {
    if !prompt_bar_enabled || self.prompt_bar_dismissed || self.prompt_count == 0 {
        return 0.0;
    }
    crate::prompt_bar::prompt_bar_height(self.prompt_count, cell_height)
}
```

- [ ] **Step 4: Run formatting and compile check after the geometry refactor.**

Run: `cargo fmt --all`
Expected: command exits with code `0`

Run: `cargo check -p scribe-client`
Expected: `Finished 'dev' profile ...` with no Rust errors

- [ ] **Step 5: Commit the geometry refactor before visual rendering.**

```bash
git add crates/scribe-client/src/prompt_bar.rs crates/scribe-client/src/pane.rs
git commit -m "refactor: centralize prompt bar geometry"
```

### Task 2: Render the Floating Card, Badge, and Bridged Dismiss Capsule

**Files:**
- Modify: `crates/scribe-client/src/prompt_bar.rs`

- [ ] **Step 1: Add rounded-rect and color helpers in `crates/scribe-client/src/prompt_bar.rs` so the renderer can build the new card without new theme fields.**

```rust
fn with_alpha(mut color: [f32; 4], alpha: f32) -> [f32; 4] {
    color[3] = alpha;
    color
}

fn mix(a: [f32; 4], b: [f32; 4], t: f32) -> [f32; 4] {
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
        a[3] + (b[3] - a[3]) * t,
    ]
}

fn push_rounded_rect(out: &mut Vec<CellInstance>, rect: Rect, color: [f32; 4], radius: f32) {
    out.push(CellInstance {
        pos: [rect.x, rect.y],
        size: [rect.width, rect.height],
        uv_min: [0.0, 0.0],
        uv_max: [0.0, 0.0],
        fg_color: color,
        bg_color: color,
        corner_radius: radius,
        _pad: 0.0,
    });
}
```

- [ ] **Step 2: Replace the flat strip rendering in `render_prompt_bar` with the floating-card shell and row hierarchy.**

```rust
pub fn render_prompt_bar(
    out: &mut Vec<CellInstance>,
    pane: &Pane,
    bar_rect: Rect,
    cell_size: (f32, f32),
    glyph_size: [f32; 2],
    hover: Option<PromptBarHover>,
    active: Option<PromptBarHover>,
    colors: &PromptBarColors,
    resolve_glyph: &mut dyn FnMut(char) -> ([f32; 2], [f32; 2]),
) {
let layout = compute_prompt_bar_layout(pane, bar_rect, cell_size).expect("prompt bar layout");
let shell_bg = mix(colors.bg, colors.first_row_bg, 0.35);
let first_row_bg = mix(colors.first_row_bg, colors.bg, 0.20);
let seam_color = with_alpha(colors.text, 0.08);

push_rounded_rect(out, layout.card_rect, shell_bg, CARD_RADIUS);
push_rounded_rect(out, layout.first_row_rect, first_row_bg, CARD_RADIUS - 2.0);
if let Some(latest_rect) = layout.latest_row_rect {
    push_rounded_rect(out, latest_rect, colors.bg, CARD_RADIUS - 2.0);
}
if let Some(seam_rect) = layout.seam_rect {
    push_solid_rect(out, seam_rect, seam_color);
}
}
```

- [ ] **Step 3: Render both prompt rows, the prompt-count badge, and the bridged dismiss capsule from the shared layout.**

```rust
fn render_count_badge(
    out: &mut Vec<CellInstance>,
    rect: Rect,
    prompt_count: u32,
    text_color: [f32; 4],
    bg_color: [f32; 4],
    cell_w: f32,
    _cell_h: f32,
    glyph_size: [f32; 2],
    resolve_glyph: &mut dyn FnMut(char) -> ([f32; 2], [f32; 2]),
) {
    push_rounded_rect(out, rect, bg_color, rect.height * 0.5);
    let label = format!("{prompt_count}x");
    let text_x = rect.x + COUNT_BADGE_PAD_X;
    let text_y = rect.y + 2.0;
    for (idx, ch) in label.chars().enumerate() {
        let x = text_x + idx as f32 * cell_w;
        emit_glyph(out, ch, x, text_y, text_color, bg_color, cell_w, glyph_size, resolve_glyph);
    }
}

fn render_dismiss_capsule(
    out: &mut Vec<CellInstance>,
    rect: Rect,
    hovered: bool,
    active: bool,
    colors: &PromptBarColors,
    cell_w: f32,
    cell_h: f32,
    glyph_size: [f32; 2],
    resolve_glyph: &mut dyn FnMut(char) -> ([f32; 2], [f32; 2]),
) {
    let capsule_bg = if active {
        with_alpha(mix(colors.bg, colors.first_row_bg, 0.55), 1.0)
    } else if hovered {
        with_alpha(mix(colors.bg, colors.first_row_bg, 0.45), 1.0)
    } else {
        with_alpha(mix(colors.bg, colors.first_row_bg, 0.35), 1.0)
    };
    push_rounded_rect(out, rect, capsule_bg, rect.height * 0.5);
    let glyph_x = rect.x + (rect.width - cell_w) * 0.5;
    let glyph_y = rect.y + (rect.height - cell_h) * 0.5;
    emit_glyph(
        out,
        '×',
        glyph_x,
        glyph_y,
        colors.text,
        capsule_bg,
        cell_w,
        glyph_size,
        resolve_glyph,
    );
}

if let Some(text) = &pane.first_prompt {
    render_prompt_line(
        out,
        ICON_FIRST,
        text,
        layout.first_row_rect.x,
        layout.first_row_rect.y + (layout.first_row_rect.height - cell_h) * 0.5,
        layout.first_text_width,
        colors.icon_first,
        colors.text,
        first_row_bg,
        cell_w,
        cell_h,
        glyph_size,
        resolve_glyph,
    );
}

if let (Some(text), Some(latest_rect), Some(text_width)) =
    (&pane.latest_prompt, layout.latest_row_rect, layout.latest_text_width)
{
    render_prompt_line(
        out,
        ICON_LATEST,
        text,
        latest_rect.x,
        latest_rect.y + (latest_rect.height - cell_h) * 0.5,
        text_width,
        colors.icon_latest,
        colors.text,
        colors.bg,
        cell_w,
        cell_h,
        glyph_size,
        resolve_glyph,
    );
}

if let Some(badge_rect) = layout.count_badge_rect {
    render_count_badge(
        out,
        badge_rect,
        pane.prompt_count,
        colors.text,
        with_alpha(colors.icon_latest, 0.18),
        cell_w,
        cell_h,
        glyph_size,
        resolve_glyph,
    );
}

render_dismiss_capsule(
    out,
    layout.dismiss_rect,
    hover == Some(PromptBarHover::DismissCapsule),
    active == Some(PromptBarHover::DismissCapsule),
    colors,
    cell_w,
    cell_h,
    glyph_size,
    resolve_glyph,
);
```

- [ ] **Step 4: Keep the row rendering restrained but obviously interactive by adding hover and pressed overlays instead of loud color shifts.**

```rust
let row_hover_overlay = with_alpha(colors.text, 0.05);
let row_active_overlay = with_alpha(colors.text, 0.09);

if hover == Some(PromptBarHover::FirstRow) {
    push_rounded_rect(out, layout.first_row_rect, row_hover_overlay, CARD_RADIUS - 2.0);
}
if active == Some(PromptBarHover::FirstRow) {
    push_rounded_rect(out, layout.first_row_rect, row_active_overlay, CARD_RADIUS - 2.0);
}
if let Some(latest_rect) = layout.latest_row_rect {
    if hover == Some(PromptBarHover::LatestRow) {
        push_rounded_rect(out, latest_rect, row_hover_overlay, CARD_RADIUS - 2.0);
    }
    if active == Some(PromptBarHover::LatestRow) {
        push_rounded_rect(out, latest_rect, row_active_overlay, CARD_RADIUS - 2.0);
    }
}
```

- [ ] **Step 5: Re-run formatting and compile check after the renderer rewrite.**

Run: `cargo fmt --all`
Expected: command exits with code `0`

Run: `cargo check -p scribe-client`
Expected: `Finished 'dev' profile ...` with no Rust errors

- [ ] **Step 6: Commit the visual renderer pass.**

```bash
git add crates/scribe-client/src/prompt_bar.rs
git commit -m "feat: render prompt bar as floating card"
```

### Task 3: Refactor Hover, Pressed State, Click Handling, and Tooltip Sizing

**Files:**
- Modify: `crates/scribe-client/src/main.rs`
- Modify: `crates/scribe-client/src/prompt_bar.rs`

- [ ] **Step 1: Introduce a shared prompt-bar target lookup in `crates/scribe-client/src/main.rs` so hover, press, copy, dismiss, and tooltip code all resolve the same target.**

```rust
fn prompt_bar_target_at(
    &self,
    x: f32,
    y: f32,
) -> Option<(PaneId, prompt_bar::PromptBarHover)> {
    let Some(gpu) = &self.gpu else { return None };
    let cell_h = gpu.renderer.cell_size().height;
    let pb_font_scale =
        self.config.terminal.prompt_bar_font_size / self.config.appearance.font_size;
    let pb_cell_h = cell_h * pb_font_scale;
    let pb_at_top =
        self.config.terminal.prompt_bar_position == scribe_common::config::PromptBarPosition::Top;

    self.visible_pane_rects().into_iter().find_map(|(pane_id, pane_rect, pane_edges)| {
        let pane = self.panes.get(&pane_id)?;
        if pane.prompt_count == 0 || pane.prompt_bar_dismissed {
            return None;
        }
        let bar_rect =
            self.prompt_bar_rect_for_visible_pane(pane, (pane_rect, pane_edges), pb_cell_h, pb_at_top)?;
        let hover = prompt_bar::hit_test_prompt_bar(
            pane,
            bar_rect,
            (gpu.renderer.cell_size().width * pb_font_scale, pb_cell_h),
            x,
            y,
        )?;
        Some((pane_id, hover))
    })
}
```

- [ ] **Step 2: Add prompt-bar pressed-state plumbing to `App`, `FrameLayout`, and `render_prompt_bar` so row clicks and dismiss clicks get visible press feedback.**

```rust
// App fields
prompt_bar_hover: Option<(PaneId, prompt_bar::PromptBarHover)>,
prompt_bar_pressed: Option<(PaneId, prompt_bar::PromptBarHover)>,

// FrameLayout
prompt_bar_hover: Option<(PaneId, prompt_bar::PromptBarHover)>,
prompt_bar_pressed: Option<(PaneId, prompt_bar::PromptBarHover)>,

// render call
prompt_bar::render_prompt_bar(
    &mut all_instances,
    pane,
    bar_rect,
    layout.prompt_bar_cell_size,
    glyph_size,
    layout.prompt_bar_hover.filter(|h| h.0 == *pane_id).map(|h| h.1),
    layout.prompt_bar_pressed.filter(|h| h.0 == *pane_id).map(|h| h.1),
    &style.prompt_bar_colors,
    &mut |ch| renderer.resolve_glyph(device, queue, ch),
);
```

- [ ] **Step 3: Update the mouse handlers in `crates/scribe-client/src/main.rs` so prompt-bar targets set and clear the pressed state at the right time.**

```rust
fn handle_mouse_press(&mut self) {
    let Some((x, y)) = self.last_cursor_pos else { return };
    let pressed_target = self.prompt_bar_target_at(x, y);
    self.prompt_bar_pressed = pressed_target;
    if pressed_target.is_some() {
        self.request_redraw();
    }

    // Keep this block immediately before the prompt-bar click checks.
    if self.try_dismiss_prompt_bar(x, y) {
        self.prompt_bar_pressed = None;
        self.request_redraw();
        return;
    }
    if self.try_copy_prompt_bar_text(x, y) {
        self.prompt_bar_pressed = None;
        self.request_redraw();
        return;
    }
    self.prompt_bar_pressed = None;
    // Status-bar, tab-bar, scrollbar, divider, and selection routing stays below this block.
}

fn handle_mouse_release(&mut self) {
    self.prompt_bar_pressed = None;
    self.request_redraw();
    self.mouse_selecting = false;
    self.finish_tab_drag();
    self.finish_pane_drag();
    if !self.config.terminal.copy_on_select {
        return;
    }
    self.finalize_copy();
    #[cfg(target_os = "linux")]
    self.set_primary_selection();
}
```

- [ ] **Step 4: Replace the duplicated hover/dismiss/copy/tooltip width logic with the shared geometry helper so truncation respects the new badge and dismiss capsule.**

```rust
pub fn hit_test_prompt_bar(
    pane: &Pane,
    bar_rect: Rect,
    cell_size: (f32, f32),
    mouse_x: f32,
    mouse_y: f32,
) -> Option<PromptBarHover> {
    let layout = compute_prompt_bar_layout(pane, bar_rect, cell_size)?;
    if layout.dismiss_rect.contains(mouse_x, mouse_y) {
        return Some(PromptBarHover::DismissCapsule);
    }
    if layout.first_row_rect.contains(mouse_x, mouse_y) {
        return Some(PromptBarHover::FirstRow);
    }
    if layout.latest_row_rect.is_some_and(|rect| rect.contains(mouse_x, mouse_y)) {
        return Some(PromptBarHover::LatestRow);
    }
    None
}

pub fn prompt_bar_text_width(
    pane: &Pane,
    bar_rect: Rect,
    cell_size: (f32, f32),
    hover: PromptBarHover,
) -> Option<f32> {
    let layout = compute_prompt_bar_layout(pane, bar_rect, cell_size)?;
    match hover {
        PromptBarHover::FirstRow => Some(layout.first_text_width),
        PromptBarHover::LatestRow => layout.latest_text_width,
        PromptBarHover::DismissCapsule => None,
    }
}
```

- [ ] **Step 5: Update the tooltip anchor path in `crates/scribe-client/src/main.rs` to use `prompt_bar_text_width` instead of `pane_rect.width - DISMISS_BTN_W`.**

```rust
let full_text = prompt_bar::hovered_prompt_text(pane, hover)?;
let anchor = layout::Rect { x: pane_rect.x, y: bar_y, width: pane_rect.width, height: pbh };
let text_width =
    prompt_bar::prompt_bar_text_width(pane, anchor, pb_cell, hover)?;
if !prompt_bar::is_prompt_truncated(full_text, text_width, pb_cell.0) {
    return None;
}
Some((full_text.to_owned(), anchor))
```

- [ ] **Step 6: Re-run formatting and compile check after the interaction refactor.**

Run: `cargo fmt --all`
Expected: command exits with code `0`

Run: `cargo check -p scribe-client`
Expected: `Finished 'dev' profile ...` with no Rust errors

- [ ] **Step 7: Commit the interaction and tooltip refactor.**

```bash
git add crates/scribe-client/src/main.rs crates/scribe-client/src/prompt_bar.rs
git commit -m "feat: improve prompt bar interactions"
```

### Task 4: Update `lat.md` and Run Manual Verification

**Files:**
- Modify: `lat.md/client.md`

- [ ] **Step 1: Update the Prompt Bar section in `lat.md/client.md` to describe the floating-card renderer, bridged dismiss capsule, shared geometry helper, and rect-based hit-testing.**

```markdown
Rendering is handled by [[crates/scribe-client/src/prompt_bar.rs#render_prompt_bar]], which now builds a rounded floating card inset within the pane rather than a flat edge-to-edge strip. Geometry is shared through [[crates/scribe-client/src/prompt_bar.rs#compute_prompt_bar_layout]], so rendering, hover hit-testing, dismissal, copy-on-click, and tooltip truncation all resolve the same row, badge, and dismiss-capsule rects.

The dismiss control is a bridged capsule that crosses the seam between the first and latest prompt rows, making it read as a whole-card action. Each row remains its own copy target with row-level hover/press feedback, and the prompt count is displayed as an informational badge rather than occupying its own text slot.
```

- [ ] **Step 2: Re-run formatting and compile check one final time before manual UI verification.**

Run: `cargo fmt --all`
Expected: command exits with code `0`

Run: `cargo check -p scribe-client`
Expected: `Finished 'dev' profile ...` with no Rust errors

- [ ] **Step 3: Manually verify the redesigned prompt bar in an existing Scribe window without restarting the server.**

Manual checklist:
1. Use a Claude Code or Codex pane that already has prompt-bar data; if needed, send enough prompts to produce both first/latest rows.
2. Hover the first row, latest row, and dismiss capsule; confirm only the hovered target highlights.
3. Press and release on the first row and latest row; confirm each target shows press feedback and still copies the full prompt text.
4. Click the bridged dismiss capsule; confirm the entire floating card hides.
5. Start a new conversation; confirm the card reappears and `prompt_bar_dismissed` resets.
6. Switch prompt-bar position between Top and Bottom in settings; confirm layout, hover, tooltip, and dismiss behavior still work in both positions.
7. Increase prompt-bar font size in settings; confirm card height, truncation, and dismiss capsule placement scale cleanly.
8. Hover a truncated prompt; confirm the tooltip still appears only when the text actually truncates.

- [ ] **Step 4: Run `lat check` after the documentation update.**

Run: `lat check`
Expected: `All checks passed`

- [ ] **Step 5: Commit the documentation and final verification pass.**

```bash
git add lat.md/client.md
git commit -m "docs: update prompt bar architecture notes"
```

## Self-Review Checklist

- Spec coverage: Task 1 covers shared geometry and height; Task 2 covers the floating card, bridged dismiss capsule, badge, and row hierarchy; Task 3 covers hover/press/copy/dismiss/tooltip behavior; Task 4 covers the required `lat.md` update and final verification.
- Placeholder scan: No `TODO`, `TBD`, or “similar to previous task” placeholders remain.
- Type consistency: Use `PromptBarHover::{FirstRow, LatestRow, DismissCapsule}` and `prompt_bar_pressed` consistently across `prompt_bar.rs` and `main.rs`.

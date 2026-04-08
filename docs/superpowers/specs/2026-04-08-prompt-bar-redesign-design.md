# Prompt Bar Floating Card Redesign

Redesign the AI prompt bar from a flat, blocky strip into a floating card with clearer hierarchy, a shared dismiss control, and stronger interaction affordances while preserving the existing prompt data model.

## Context

The current prompt bar succeeds functionally but looks dated. Its flat rectangular blocks, narrow left-edge dismiss zone, and single `×` glyph make the component feel heavy and visually ambiguous. The dismiss affordance in particular reads like it belongs to one row even though it closes the entire bar.

The goal of this redesign is not to add new product surface area. It is to make the existing prompt bar feel intentional, modern, and structurally coherent inside Scribe's terminal chrome.

## Goals

- Replace the current flat bar with a more modern floating card treatment.
- Make the dismiss control clearly belong to the whole component, not an individual row.
- Improve hover, press, and click affordances so copy and dismiss actions feel obvious.
- Keep the existing prompt model intact: first prompt, latest prompt, prompt count, copy on row click, dismiss until conversation reset.
- Preserve prompt bar top/bottom placement and existing prompt-bar settings.

## Non-Goals

- No new prompt history view or expanded prompt browser.
- No new settings for style variants or extra prompt-bar controls.
- No change to how prompts are collected, stored, dismissed, or reset across conversations.

## Chosen Direction

The prompt bar will become a floating card rather than two exposed stacked strips. The chosen direction is the "Floating Card" approach: a calmer overall visual tone with a materially upgraded layout and interaction model.

The dismiss control will use a bridged capsule treatment. Instead of living inside a left strip or looking attached to one row, it visually crosses the seam between the first and latest prompt rows so it reads as one shared action for the whole component.

The redesign is intentionally broader than a reskin. It should feel like premium terminal chrome rather than a recolored version of the current bar.

## Visual Design

### Outer Shell

The component should render as one rounded, slightly inset card inside the pane rather than edge-to-edge blocks. The shell should use a soft terminal-friendly gradient or tonal blend derived from existing prompt bar theme colors, not a brand-new palette.

Key traits:

- Rounded outer corners on the full card.
- Slight inset from pane edges so the card feels placed rather than stamped on.
- Subtle separation from surrounding content through tone, not loud contrast.
- A faint seam between prompt rows so they read as related sections inside one unit.

### Row Hierarchy

The first and latest prompt rows remain distinct, but they should feel like sections of one card rather than separate bars. The first row can carry slightly stronger emphasis to anchor the component, while the latest row should remain clearly interactive and easy to scan.

The icons for first/latest should be cleaner and more deliberate than the current glyph treatment. They should help orient the user without pulling more attention than the prompt text.

### Dismiss Control

The dismiss control should be a single shared capsule that bridges the two rows across their seam. It must look explicitly attached to the whole card.

Visual expectations:

- Compact, not oversized.
- Clearly clickable.
- Balanced against the card, not decorative.
- Strong enough to read immediately, but restrained enough to fit Scribe's technical UI.

### Prompt Count

Prompt count should move into a small badge inside the card rather than feeling like leftover text or layout filler. The badge is informational only in this pass.

## Interaction Design

### Row Actions

Each prompt row remains its own copy target. The card itself is not a single giant click area. Hovering a row should reveal that the row is interactive through a full-row hover state, not just subtle text changes.

Pressed feedback should be clearer than it is today so copy-on-click feels intentional rather than accidental.

### Dismiss Action

The bridged capsule is the dedicated dismiss affordance and should always dismiss the entire card. It should have its own hover and pressed states distinct from row hover.

This resolves the current structural mismatch where the X appears attached to one part of the bar while performing a whole-component action.

### Truncation And Tooltip

Prompt text should continue truncating with ellipsis when space runs out. Existing tooltip behavior for truncated content should remain available, but the redesign should ensure the card still reads cleanly when the user does not hover.

## Layout And Rendering Constraints

This work should stay inside the existing prompt-bar architecture rather than becoming a new overlay system.

Primary implementation touchpoints:

- `crates/scribe-client/src/prompt_bar.rs`
- `crates/scribe-client/src/main.rs`
- `crates/scribe-client/src/pane.rs`
- `lat.md/client.md`

Expected renderer changes:

- Replace the flat strip rendering with a rounded card shell and internal seam.
- Add explicit geometry for first row, latest row, dismiss capsule, and count badge.
- Update hover hit-testing to use concrete interactive rects instead of the current left-edge dismiss strip.
- Keep prompt bar top/bottom placement logic and conversation-reset behavior unchanged.

Expected layout changes:

- Allow more internal padding and a slightly more generous height if needed.
- Keep the bar compact enough that it still feels like chrome, not a separate panel.
- Preserve existing font-size scaling behavior for the prompt bar.

## Configuration And Theming

The redesign should continue deriving from the existing prompt bar colors in theme chrome and appearance overrides. It should not introduce new user-facing prompt-bar style settings in this pass.

The implementation may reinterpret the existing five prompt-bar colors across the new card shell, seam, text, icons, and dismiss treatment, but it should not break current customization behavior.

## Acceptance Criteria

- The prompt bar no longer appears as a flat, blocky strip.
- The dismiss control clearly reads as a shared whole-card action.
- Row copy targets are visually obvious on hover and press.
- Prompt count is integrated cleanly into the card.
- The component still supports top and bottom positioning.
- Dismiss, copy, truncation, tooltips, and conversation-reset behavior still work.
- No new prompt-bar settings are required to access the redesign.

## Verification

Manual verification should cover:

- Single-prompt and multi-prompt states.
- Hover and click behavior for first row, latest row, and dismiss capsule.
- Top and bottom prompt-bar positions.
- Different prompt bar font sizes.
- Long prompt truncation and tooltip behavior.
- Existing prompt dismissal and conversation reset flows.

If implementation risk grows, the right response is to simplify styling details, not to collapse back to the old left-strip dismiss model.

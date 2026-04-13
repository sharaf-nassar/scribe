# Explicit AI Tab Shortcuts Design

This spec replaces provider-selected AI tab shortcuts with explicit Claude Code and Codex open/resume actions across config, UI, command palette, and runtime dispatch.

## Problem

The current model exposes two AI tab shortcuts, but their actual behavior depends on the separate `terminal.ai_tab_provider` setting. That makes the shortcuts ambiguous, forces the settings UI to relabel them dynamically, and hides the real launch behavior behind a separate toggle.

The requested feature is to make the shortcut intent explicit: one open/resume pair for Claude Code and one open/resume pair for Codex, while keeping the current shortcut pair on Claude and adding a new Codex pair.

## Goals

- Make AI tab shortcuts provider-explicit instead of provider-selected.
- Keep the current AI shortcut defaults on Claude Code.
- Add a dedicated Codex open/resume shortcut pair.
- Remove the `AI Tab Provider` setting from the settings UI.
- Make command palette entries match the explicit shortcut model.
- Preserve config-load compatibility for existing installs without requiring manual migration.

## Non-Goals

- Renaming the existing Claude-facing config keys for symmetry.
- Adding new automated tests unless requested separately.
- Changing replay/restore semantics beyond whatever is needed to keep explicit Claude and Codex launches working.

## Chosen Approach

Keep the existing `new_claude_tab` and `new_claude_resume_tab` config keys and treat them as explicit Claude Code actions going forward.

Add parallel `new_codex_tab` and `new_codex_resume_tab` keybinding fields, layout actions, and command palette actions. Runtime dispatch chooses Claude or Codex directly from the action that fired, not from `terminal.ai_tab_provider`.

The `terminal.ai_tab_provider` field remains load-compatible in config parsing for now, but it becomes inert compatibility state and no longer affects AI shortcut behavior.

## Defaults

### Non-macOS

- Claude open: `ctrl+alt+c`
- Claude resume: `ctrl+alt+r`
- Codex open: `ctrl+alt+x`
- Codex resume: `ctrl+alt+e`

### macOS

- Claude open: `ctrl+alt+c`
- Claude resume: `ctrl+alt+r`
- Codex open: `cmd+alt+x`
- Codex resume: `cmd+alt+e`

Claude defaults stay unchanged across platforms for compatibility, while Codex uses macOS `cmd+alt` equivalents.

## UI Behavior

### AI Settings Page

Remove the `AI Tab Provider` segmented control entirely. The AI page should no longer imply that one toggle selects which provider the shortcuts launch.

### Keybindings Page

Render four explicit AI rows:

- `New Claude Tab`
- `Resume Claude Tab`
- `New Codex Tab`
- `Resume Codex Tab`

The rows use the normal keybinding editor and reset-to-default behavior. No provider-driven relabeling remains in the webview JavaScript.

### Command Palette

Render four static AI entries:

- `New Claude Tab`
- `Resume Claude Tab`
- `New Codex Tab`
- `Resume Codex Tab`

These entries should invoke the same explicit runtime actions as the keyboard shortcuts so every user-facing launch surface stays aligned.

## Runtime Behavior

Client input maps each configured shortcut directly to an explicit layout action:

- Claude open
- Claude resume
- Codex open
- Codex resume

`main.rs` keeps one shared helper for building AI launch commands, but that helper takes `provider` plus `resume` instead of reading `terminal.ai_tab_provider`.

The actual shell commands remain:

- Claude open: `exec claude`
- Claude resume: `exec claude --resume`
- Codex open: `exec codex`
- Codex resume: `exec codex resume`

## Data Flow

1. Config deserializes the existing Claude keybindings and the new Codex keybindings.
2. Settings UI receives default keybinding values from Rust and renders four explicit AI shortcut rows.
3. Settings edits persist directly to one of the four explicit keybinding fields.
4. Client input translates those bindings into one of four explicit layout actions.
5. Layout action dispatch invokes the shared AI launch helper with an explicit provider and resume mode.
6. Command palette actions use the same explicit action path as the keyboard shortcuts.
7. Replay and restore continue to identify AI sessions from the actual launched argv, so explicit Claude and Codex tabs keep their provider/resume identity.

## Compatibility

Old configs remain readable.

- Existing `new_claude_tab` and `new_claude_resume_tab` values keep working and now unambiguously mean Claude Code.
- Missing `new_codex_tab` and `new_codex_resume_tab` fields pick up the new defaults automatically.
- Existing `terminal.ai_tab_provider` values are tolerated during config load, but they no longer affect runtime shortcut dispatch or command palette labeling.

No heuristic migration based on the old provider setting is performed. Users who had previously pointed the old provider-selected shortcuts at Codex will now use the explicit Codex shortcuts instead.

## Risks and Mitigations

### Compatibility Surprise For Existing Codex Users

Users who relied on the old provider toggle to make `ctrl+alt+c` or `ctrl+alt+r` launch Codex will now get Claude Code on those bindings. This is intentional, but the settings UI should make the new Codex bindings obvious so the replacement path is immediately visible.

### Surface Drift

If keybindings, command palette actions, and runtime dispatch are not updated together, the UI will become inconsistent. The implementation should update all user-facing launch surfaces in the same patch.

### Dead Legacy State

Keeping `terminal.ai_tab_provider` load-compatible but inactive can confuse future maintenance if it looks live. The code and docs should mark it as compatibility-only and remove user-facing references immediately.

## Verification

- `cargo check -p scribe-common -p scribe-client -p scribe-settings`
- Manual inspection of the settings UI to confirm:
  - the `AI Tab Provider` control is removed
  - four explicit AI shortcut rows are present
  - reset-to-default shows the expected platform defaults
- Manual inspection of command palette items to confirm all four explicit AI actions exist
- Manual inspection of runtime dispatch to confirm each action launches the expected CLI command
- `lat check`

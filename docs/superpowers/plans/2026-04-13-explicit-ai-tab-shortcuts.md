# Explicit AI Tab Shortcuts Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace provider-selected AI tab shortcuts with explicit Claude Code and Codex open/resume actions across config, runtime, command palette, and settings UI.

**Architecture:** Keep the existing `new_claude_*` keys as explicit Claude Code actions for compatibility, add parallel `new_codex_*` actions, and route every user-facing launch surface through one provider-aware helper in the client. Remove the `AI Tab Provider` control from the settings UI, keep `terminal.ai_tab_provider` load-compatible in config only, and update `lat.md` so the knowledge graph matches the new explicit behavior.

**Tech Stack:** Rust (`scribe-common`, `scribe-client`, `scribe-settings`), embedded HTML/JS settings UI, `lat.md` documentation

---

## Implementation Constraints

- Do not add new tests unless the user explicitly asks for them.
- Do not restart the Scribe server.
- Make one final commit after all code, docs, and verification are complete.

## File Structure

- Modify: `crates/scribe-common/src/config.rs`
  - Add explicit Codex keybinding fields and defaults.
  - Mark `terminal.ai_tab_provider` as compatibility-only state.
- Modify: `crates/scribe-common/src/protocol.rs`
  - Add explicit Codex automation actions so command palette behavior matches the new shortcut split.
- Modify: `crates/scribe-client/src/input.rs`
  - Parse the new Codex bindings and emit explicit Claude/Codex layout actions.
- Modify: `crates/scribe-client/src/main.rs`
  - Route layout and automation actions through one helper that selects the CLI from `AiProvider` plus resume mode.
  - Make command palette labels static and explicit.
- Modify: `crates/scribe-settings/src/assets/settings.html`
  - Remove the `AI Tab Provider` control and render four explicit AI shortcut rows.
- Modify: `crates/scribe-settings/src/assets/settings.js`
  - Remove provider-driven relabeling and keep only generic keybinding rendering.
- Modify: `crates/scribe-settings/src/apply.rs`
  - Persist `new_codex_tab` and `new_codex_resume_tab`.
  - Remove the now-unused `terminal.ai_tab_provider` settings-edit path.
- Modify: `lat.md/common.md`
  - Update the shared keybinding description to mention explicit Claude/Codex AI actions.
- Modify: `lat.md/client.md`
  - Update layout-action and command-palette descriptions to reflect explicit AI actions.
- Modify: `lat.md/settings.md`
  - Remove provider-selected shortcut wording and describe the four explicit AI keybindings.
- Keep unchanged after inspection: `crates/scribe-client/src/restore_replay.rs`
  - The existing argv-based detection should already recognize `exec claude`, `exec claude --resume`, `exec codex`, and `exec codex resume`.

### Task 1: Extend Shared Config And Automation Actions

**Files:**
- Modify: `crates/scribe-common/src/config.rs`
- Modify: `crates/scribe-common/src/protocol.rs`

- [ ] **Step 1: Add explicit Codex keybinding fields and defaults in `config.rs`**

```rust
    #[serde(default = "default_new_claude_tab")]
    pub new_claude_tab: KeyComboList,
    #[serde(default = "default_new_claude_resume_tab")]
    pub new_claude_resume_tab: KeyComboList,
    #[serde(default = "default_new_codex_tab")]
    pub new_codex_tab: KeyComboList,
    #[serde(default = "default_new_codex_resume_tab")]
    pub new_codex_resume_tab: KeyComboList,
```

```rust
            new_claude_tab: default_new_claude_tab(),
            new_claude_resume_tab: default_new_claude_resume_tab(),
            new_codex_tab: default_new_codex_tab(),
            new_codex_resume_tab: default_new_codex_resume_tab(),
```

```rust
fn default_new_claude_tab() -> KeyComboList {
    KeyComboList::single("ctrl+alt+c")
}

fn default_new_claude_resume_tab() -> KeyComboList {
    KeyComboList::single("ctrl+alt+r")
}

fn default_new_codex_tab() -> KeyComboList {
    platform_combo("cmd+alt+x", "ctrl+alt+x")
}

fn default_new_codex_resume_tab() -> KeyComboList {
    platform_combo("cmd+alt+e", "ctrl+alt+e")
}
```

- [ ] **Step 2: Mark `terminal.ai_tab_provider` as compatibility-only state**

```rust
    /// Legacy compatibility field kept so older configs still deserialize.
    /// AI tab shortcuts now bind Claude Code and Codex explicitly.
    #[serde(default = "default_ai_tab_provider")]
    pub ai_tab_provider: AiProvider,
```

- [ ] **Step 3: Add explicit Codex automation actions for command-palette parity**

```rust
pub enum AutomationAction {
    OpenSettings,
    OpenFind,
    NewTab,
    NewClaudeTab,
    NewClaudeResumeTab,
    NewCodexTab,
    NewCodexResumeTab,
    SplitVertical,
    SplitHorizontal,
    ClosePane,
    CloseTab,
    NewWindow,
    SwitchProfile { name: String },
    OpenUpdateDialog,
}
```

- [ ] **Step 4: Verify the shared crates still compile**

Run: `cargo check -p scribe-common`

Expected: `cargo check` finishes without errors after the new fields, defaults, and enum variants are added.

### Task 2: Route Explicit AI Actions Through Client Input And Launching

**Files:**
- Modify: `crates/scribe-client/src/input.rs`
- Modify: `crates/scribe-client/src/main.rs`
- Inspect only: `crates/scribe-client/src/restore_replay.rs`

- [ ] **Step 1: Extend `Bindings` and `LayoutAction` for the new Codex actions**

```rust
    pub new_tab: BindingSet,
    pub new_claude_tab: BindingSet,
    pub new_claude_resume_tab: BindingSet,
    pub new_codex_tab: BindingSet,
    pub new_codex_resume_tab: BindingSet,
```

```rust
            new_tab: parse_set(&config.new_tab),
            new_claude_tab: parse_set(&config.new_claude_tab),
            new_claude_resume_tab: parse_set(&config.new_claude_resume_tab),
            new_codex_tab: parse_set(&config.new_codex_tab),
            new_codex_resume_tab: parse_set(&config.new_codex_resume_tab),
```

```rust
    NewTab,
    NewClaudeTab,
    NewClaudeResumeTab,
    NewCodexTab,
    NewCodexResumeTab,
    CloseTab,
```

- [ ] **Step 2: Map the new Codex bindings in `translate_layout_shortcut()`**

```rust
    if any_matches(&bindings.new_claude_tab, event, modifiers) {
        return Some(LayoutAction::NewClaudeTab);
    }
    if any_matches(&bindings.new_claude_resume_tab, event, modifiers) {
        return Some(LayoutAction::NewClaudeResumeTab);
    }
    if any_matches(&bindings.new_codex_tab, event, modifiers) {
        return Some(LayoutAction::NewCodexTab);
    }
    if any_matches(&bindings.new_codex_resume_tab, event, modifiers) {
        return Some(LayoutAction::NewCodexResumeTab);
    }
```

- [ ] **Step 3: Replace provider-selected launching with one explicit helper in `main.rs`**

```rust
    fn ai_tab_command(&self, provider: AiProvider, resume: bool) -> Vec<String> {
        let shell = scribe_common::shell::default_shell_program();
        let command = match (provider, resume) {
            (AiProvider::ClaudeCode, false) => String::from("exec claude"),
            (AiProvider::ClaudeCode, true) => String::from("exec claude --resume"),
            (AiProvider::CodexCode, false) => String::from("exec codex"),
            (AiProvider::CodexCode, true) => String::from("exec codex resume"),
        };
        vec![shell, String::from("-lic"), command]
    }
```

```rust
    fn handle_new_codex_tab(&mut self) {
        let project_root = self.focused_workspace_project_root();
        self.create_new_tab(Some(self.ai_tab_command(AiProvider::CodexCode, false)), project_root);
    }

    fn handle_new_codex_resume_tab(&mut self) {
        let project_root = self.focused_workspace_project_root();
        self.create_new_tab(Some(self.ai_tab_command(AiProvider::CodexCode, true)), project_root);
    }
```

- [ ] **Step 4: Update layout dispatch, automation dispatch, and command-palette labels**

```rust
            LayoutAction::NewClaudeTab => self.handle_new_claude_tab(),
            LayoutAction::NewClaudeResumeTab => self.handle_new_claude_resume_tab(),
            LayoutAction::NewCodexTab => self.handle_new_codex_tab(),
            LayoutAction::NewCodexResumeTab => self.handle_new_codex_resume_tab(),
```

```rust
            AutomationAction::NewClaudeTab => self.handle_new_claude_tab(),
            AutomationAction::NewClaudeResumeTab => self.handle_new_claude_resume_tab(),
            AutomationAction::NewCodexTab => self.handle_new_codex_tab(),
            AutomationAction::NewCodexResumeTab => self.handle_new_codex_resume_tab(),
```

```rust
            CommandPaletteEntry {
                label: String::from("New Claude Tab"),
                action: AutomationAction::NewClaudeTab,
            },
            CommandPaletteEntry {
                label: String::from("Resume Claude Tab"),
                action: AutomationAction::NewClaudeResumeTab,
            },
            CommandPaletteEntry {
                label: String::from("New Codex Tab"),
                action: AutomationAction::NewCodexTab,
            },
            CommandPaletteEntry {
                label: String::from("Resume Codex Tab"),
                action: AutomationAction::NewCodexResumeTab,
            },
```

- [ ] **Step 5: Confirm replay detection needs no code change**

Read `crates/scribe-client/src/restore_replay.rs` and confirm the existing `is_ai_command(argv, provider, resume)` logic already matches the four concrete shell commands above. Leave the file unchanged unless the compiler proves otherwise.

- [ ] **Step 6: Verify the client crates compile**

Run: `cargo check -p scribe-client -p scribe-common`

Expected: `cargo check` finishes without errors and no `match` arms remain non-exhaustive for `LayoutAction` or `AutomationAction`.

### Task 3: Replace Provider-Selected Settings UI With Explicit Shortcut Rows

**Files:**
- Modify: `crates/scribe-settings/src/assets/settings.html`
- Modify: `crates/scribe-settings/src/assets/settings.js`
- Modify: `crates/scribe-settings/src/apply.rs`

- [ ] **Step 1: Remove the AI Tab Provider row from the AI page**

Delete the `setting-row` that renders:

```html
<div class="setting-label">AI Tab Provider</div>
<div class="segmented-control" data-key="terminal.ai_tab_provider">
```

The AI page should keep the remaining integration toggles, prompt-bar controls, and indicator settings unchanged.

- [ ] **Step 2: Render four explicit AI shortcut rows on the Keybindings page**

```html
<div class="setting-row">
  <div class="setting-info"><div class="setting-label">New Claude Tab</div></div>
  <div class="keybinding-cell" data-action="new_claude_tab">
    <button class="kb-reset-btn" data-action="new_claude_tab" title="Reset to default">&#8635;</button>
  </div>
</div>
<div class="setting-row">
  <div class="setting-info"><div class="setting-label">Resume Claude Tab</div></div>
  <div class="keybinding-cell" data-action="new_claude_resume_tab">
    <button class="kb-reset-btn" data-action="new_claude_resume_tab" title="Reset to default">&#8635;</button>
  </div>
</div>
<div class="setting-row">
  <div class="setting-info"><div class="setting-label">New Codex Tab</div></div>
  <div class="keybinding-cell" data-action="new_codex_tab">
    <button class="kb-reset-btn" data-action="new_codex_tab" title="Reset to default">&#8635;</button>
  </div>
</div>
<div class="setting-row">
  <div class="setting-info"><div class="setting-label">Resume Codex Tab</div></div>
  <div class="keybinding-cell" data-action="new_codex_resume_tab">
    <button class="kb-reset-btn" data-action="new_codex_resume_tab" title="Reset to default">&#8635;</button>
  </div>
</div>
```

- [ ] **Step 3: Remove provider-driven label logic from `settings.js`**

Delete `normalizeAiTabProvider()`, `renderAiTabLabels()`, and `setAiTabProvider()`, and remove the `terminal.ai_tab_provider` special case from `initSegmented()`.

```js
function initSegmented() {
  document.querySelectorAll(".segmented-control").forEach(function(ctrl) {
    const key = ctrl.getAttribute("data-key");
    const opts = ctrl.querySelectorAll(".segment-opt");

    opts.forEach(function(opt) {
      opt.addEventListener("click", function() {
        var value = opt.getAttribute("data-value");
        opts.forEach(function(o) { o.classList.remove("active"); });
        opt.classList.add("active");
        sendChange(key, value);
      });
    });
  });
}
```

Also remove the config-load call that currently does:

```js
setAiTabProvider(config.terminal?.ai_tab_provider);
```

- [ ] **Step 4: Persist the new Codex keybinding fields in `apply.rs`**

```rust
        "new_tab" => kb.new_tab = list,
        "new_claude_tab" => kb.new_claude_tab = list,
        "new_claude_resume_tab" => kb.new_claude_resume_tab = list,
        "new_codex_tab" => kb.new_codex_tab = list,
        "new_codex_resume_tab" => kb.new_codex_resume_tab = list,
        "close_tab" => kb.close_tab = list,
```

Delete the `terminal.ai_tab_provider` match arm from `apply_setting_change()` so the settings app no longer exposes a user-edit path for that legacy field.

- [ ] **Step 5: Verify settings, client, and common still compile together**

Run: `cargo check -p scribe-settings -p scribe-client -p scribe-common`

Expected: `cargo check` finishes without errors and no stale references to `setAiTabProvider`, `data-ai-tab-label`, or `terminal.ai_tab_provider` remain in the settings package.

### Task 4: Update The `lat.md` Knowledge Graph

**Files:**
- Modify: `lat.md/common.md`
- Modify: `lat.md/client.md`
- Modify: `lat.md/settings.md`

- [ ] **Step 1: Update `lat.md/common.md` to mention explicit AI keybindings**

Replace the generic keybinding sentence with wording that calls out the explicit AI actions:

```md
[[crates/scribe-common/src/config.rs#KeybindingsConfig]] exposes 50+ configurable actions across pane navigation, workspace splits, tab management, explicit Claude Code and Codex open/resume shortcuts, clipboard, scrolling, zoom, and terminal word-motion shortcuts.
```

- [ ] **Step 2: Update `lat.md/client.md` layout-action wording**

Replace the provider-selected AI shortcut wording with:

```md
Tab actions: new, new/resume Claude Code, new/resume Codex, close, next, prev, select 1-9. The legacy `new_claude_*` config keys now map directly to Claude Code, while `new_codex_*` keys open Codex. All AI-tab shortcuts start the selected CLI through the user's login shell with `-lic` and `exec`, resolving the shell from `SHELL` first and then the account database so Finder-launched macOS apps still inherit the expected PATH and rc files without first rendering a normal shell prompt.
```

If the command-palette section mentions provider-selected AI actions, update that text to say the palette exposes explicit Claude Code and Codex tab actions.

- [ ] **Step 3: Update `lat.md/settings.md` AI and keybinding sections**

Replace the provider-toggle wording in `AI Keys` with:

```md
Clipboard cleanup remains persisted as `claude_copy_cleanup` for backward compatibility. `terminal.ai_tab_provider` remains load-compatible for older configs, but AI tab shortcuts are now configured explicitly through `new_claude_tab`, `new_claude_resume_tab`, `new_codex_tab`, and `new_codex_resume_tab`.
```

Replace the `Keybinding Keys` paragraph with:

```md
Actions cover: pane splits, focus directions, workspace splits, workspace cycling, tab management (new, new/resume Claude Code, new/resume Codex, close, next, prev, select 1-9), clipboard, scrolling, command palette, find, zoom, settings, new window, and terminal shortcuts (word left/right, delete word, line start/end).
```

- [ ] **Step 4: Run `lat check` after the doc edits**

Run: `lat check`

Expected: the command exits successfully with no invalid wiki links, missing leading paragraphs, or broken code references.

### Task 5: Final Verification And Single Commit

**Files:**
- Modify: none

- [ ] **Step 1: Run the full compile verification pass**

Run: `cargo check -p scribe-common -p scribe-client -p scribe-settings`

Expected: all three packages compile successfully with no warnings elevated to errors and no missing match arms for the new explicit actions.

- [ ] **Step 2: Review the final diff shape**

Run: `git diff --stat`

Expected: the diff is limited to the shared config, client input/runtime, settings UI/apply path, `lat.md` docs, and the approved design/plan docs.

- [ ] **Step 3: Read recent commit messages before drafting the final commit**

Run: `git log --format=%B -n 5`

Expected: recent subjects and bodies are visible so the final commit message can follow the repo's current style.

- [ ] **Step 4: Create one final commit for the full feature**

Run:

```bash
git add crates/scribe-common/src/config.rs \
        crates/scribe-common/src/protocol.rs \
        crates/scribe-client/src/input.rs \
        crates/scribe-client/src/main.rs \
        crates/scribe-settings/src/assets/settings.html \
        crates/scribe-settings/src/assets/settings.js \
        crates/scribe-settings/src/apply.rs \
        lat.md/common.md \
        lat.md/client.md \
        lat.md/settings.md \
        docs/superpowers/specs/2026-04-13-explicit-ai-tab-shortcuts-design.md \
        docs/superpowers/plans/2026-04-13-explicit-ai-tab-shortcuts.md
git commit -m "feat: split Claude and Codex AI tab shortcuts" \
  -m "Replace provider-selected AI tab behavior with explicit Claude Code and Codex open and resume actions." \
  -m "Remove the AI Tab Provider settings control, add dedicated Codex keybindings and command-palette actions, and route launches directly from explicit actions while keeping legacy Claude bindings compatible."
```

Expected: git creates one commit containing the code, `lat.md`, spec, and plan changes.

## Self-Review

- Spec coverage: Tasks 1-3 implement the config, runtime, command palette, and settings UI changes from the approved spec. Task 4 covers the required `lat.md` updates and `lat check`. Task 5 covers verification and the single final commit.
- Placeholder scan: No `TODO`, `TBD`, or "similar to above" placeholders remain. Every code-changing task includes concrete snippets, exact file paths, and exact commands.
- Type consistency: The plan uses one naming scheme throughout: `new_claude_tab`, `new_claude_resume_tab`, `new_codex_tab`, `new_codex_resume_tab`, `AutomationAction::NewCodexTab`, `AutomationAction::NewCodexResumeTab`, `LayoutAction::NewCodexTab`, and `LayoutAction::NewCodexResumeTab`.

# Tasks: Smart Selection

**Input**: Design documents from `specs/002-smart-selection/`
**Prerequisites**: [plan.md](./plan.md), [spec.md](./spec.md), [research.md](./research.md), [data-model.md](./data-model.md), [contracts/](./contracts/)

**Tests**: No new test-code tasks are included because the project instruction says to write test code only when explicitly requested. Verification tasks use existing package checks and the quickstart manual scenarios.

**Organization**: Tasks are grouped by user story so each story can be implemented and verified independently.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: Can run in parallel with other tasks in the same phase because it touches different files and does not depend on incomplete work
- **[Story]**: User story label for story phases only
- Every task includes an exact file path

## Phase 1: Setup (Shared Infrastructure)

**Purpose**: Prepare dependencies and module structure used by all Smart Selection stories.

- [X] T001 Add the workspace `regex` dependency to `scribe-client` in `crates/scribe-client/Cargo.toml`
- [X] T002 Declare the client `smart_selection` module in `crates/scribe-client/src/main.rs`
- [X] T003 Create the empty Smart Selection implementation module in `crates/scribe-client/src/smart_selection.rs`
- [X] T004 [P] Add Smart Selection notes to the Terminal config area in `crates/scribe-common/src/config.rs`

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: Define shared data shapes, defaults, and client-side compilation primitives required before any user story can work.

**CRITICAL**: No user story work should begin until this phase is complete.

- [X] T005 Define `SmartSelectionConfig`, `SmartSelectionActivation`, `SmartSelectionRule`, `SmartSelectionPrecision`, `SmartSelectionAction`, `SmartSelectionActionKind`, and `SmartSelectionParameterMode` in `crates/scribe-common/src/config.rs`
- [X] T006 Implement default Smart Selection config with iTerm2-style default recognizers in `crates/scribe-common/src/config.rs`
- [X] T007 Add `smart_selection` to `TerminalConfig` defaults and serde loading in `crates/scribe-common/src/config.rs`
- [X] T008 Implement compiled rule structures, validation result structures, and config-to-compiled-rule conversion in `crates/scribe-client/src/smart_selection.rs`
- [X] T009 Implement logical visible text collection with terminal grid row/column mapping in `crates/scribe-client/src/smart_selection.rs`
- [X] T010 Implement candidate scoring by cursor containment, per-rule longest match, precision, and match length in `crates/scribe-client/src/smart_selection.rs`
- [X] T011 Add Smart Selection cache/recompile state to the `App` struct in `crates/scribe-client/src/main.rs`
- [X] T012 Rebuild Smart Selection compiled rules on startup and config reload in `crates/scribe-client/src/main.rs`
- [X] T013 Update `ConfigReloadPlan` to recognize Smart Selection config changes in `crates/scribe-client/src/main.rs`

**Checkpoint**: Smart Selection settings can load, defaults exist, and the client can compile and score rules without changing click behavior yet.

---

## Phase 3: User Story 1 - Select Semantic Text Quickly (Priority: P1) MVP

**Goal**: A user can invoke Smart Selection by the configured gesture and get the intended semantic object selected under the cursor.

**Independent Test**: Configure Smart Selection for quad click, display URLs, paths, quoted strings, include paths, selectors, email addresses, namespace identifiers, and plain words in the terminal, then quad-click inside each object and verify only the intended object is selected.

### Implementation for User Story 1

- [X] T014 [US1] Extend `ClickKind` and click counting to recognize quad click in `crates/scribe-client/src/mouse_state.rs`
- [X] T015 [US1] Add Smart Selection activation dispatch that chooses double click or quad click based on config in `crates/scribe-client/src/main.rs`
- [X] T016 [US1] Implement `start_selection_smart` to map cursor pixels to a Smart Selection candidate in `crates/scribe-client/src/main.rs`
- [X] T017 [US1] Add Smart Selection range construction that reuses `SelectionRange` highlighting in `crates/scribe-client/src/selection.rs`
- [X] T018 [US1] Preserve existing double-click word selection when Smart Selection activation is `quad_click` in `crates/scribe-client/src/main.rs`
- [X] T019 [US1] Replace double-click word selection when Smart Selection activation is `double_click` in `crates/scribe-client/src/main.rs`
- [X] T020 [US1] Preserve existing triple-click line selection after adding quad-click handling in `crates/scribe-client/src/main.rs`
- [X] T021 [US1] Preserve existing Shift mouse-selection bypass for mouse-reporting applications in `crates/scribe-client/src/main.rs`
- [X] T022 [US1] Preserve copy-on-select and primary-selection behavior for Smart Selection selections in `crates/scribe-client/src/main.rs`
- [ ] T023 [US1] Verify US1 manually with the examples in `specs/002-smart-selection/quickstart.md`

**Checkpoint**: Smart Selection can be used from terminal clicks with default rules and no settings UI changes beyond config defaults.

---

## Phase 4: User Story 2 - Configure Smart Selection Rules (Priority: P2)

**Goal**: A user can configure the activation gesture and manage Smart Selection rules from a dedicated Terminal settings section.

**Independent Test**: Open Terminal settings, edit a rule regex and precision, add a new rule, save, reopen settings, and verify the saved rules apply in every terminal pane.

### Implementation for User Story 2

- [X] T024 [US2] Add `terminal.smart_selection` and `terminal.smart_selection.reset` handling in `crates/scribe-settings/src/apply.rs`
- [X] T025 [US2] Add serde parsing and validation for full Smart Selection settings payloads in `crates/scribe-settings/src/apply.rs`
- [X] T026 [US2] Add a dedicated Smart Selection section after General on the Terminal page in `crates/scribe-settings/src/assets/settings.html`
- [X] T027 [US2] Add activation segmented control markup for Double Click and Quad Click in `crates/scribe-settings/src/assets/settings.html`
- [X] T028 [US2] Add rule-list markup and selected-rule editor containers in `crates/scribe-settings/src/assets/settings.html`
- [X] T029 [US2] Add action-list editor markup for Smart Selection rule actions in `crates/scribe-settings/src/assets/settings.html`
- [X] T030 [US2] Add Smart Selection settings state loading from `currentConfig.terminal.smart_selection` in `crates/scribe-settings/src/assets/settings.js`
- [X] T031 [US2] Implement rule add, duplicate, remove, enable, disable, move up, move down, and restore-defaults handlers in `crates/scribe-settings/src/assets/settings.js`
- [X] T032 [US2] Implement rule editor updates for name, regex, and precision in `crates/scribe-settings/src/assets/settings.js`
- [X] T033 [US2] Implement settings-side regex validation and inline error rendering in `crates/scribe-settings/src/assets/settings.js`
- [X] T034 [US2] Implement sample-text Smart Selection preview for a selected rule in `crates/scribe-settings/src/assets/settings.js`
- [X] T035 [US2] Send full `terminal.smart_selection` payloads and reset payloads from settings UI in `crates/scribe-settings/src/assets/settings.js`
- [X] T036 [US2] Style the Smart Selection section, rule list, editor states, validation states, and empty state in `crates/scribe-settings/src/assets/settings.css`
- [X] T037 [US2] Ensure settings reload displays saved activation, rules, precision values, and actions in `crates/scribe-settings/src/assets/settings.js`
- [ ] T038 [US2] Verify US2 manually with the settings flow in `specs/002-smart-selection/quickstart.md`

**Checkpoint**: Smart Selection rules and activation are configurable globally through Terminal settings and apply to already-open panes.

---

## Phase 5: User Story 3 - Invoke Rule Actions (Priority: P3)

**Goal**: Matching Smart Selection rules contribute explicit context-menu actions that execute only when the user selects an action.

**Independent Test**: Configure actions on a Smart Selection rule, right-click matching text in the terminal, verify those actions appear, and confirm each action executes only after explicit selection.

### Implementation for User Story 3

- [X] T039 [US3] Extend context-menu data to carry Smart Selection rule actions in `crates/scribe-client/src/context_menu.rs`
- [X] T040 [US3] Add Smart Selection context-menu lookup over the cursor location in `crates/scribe-client/src/main.rs`
- [X] T041 [US3] Implement legacy action parameter expansion for `\0`, `\1`-`\9`, `\d`, `\u`, `\h`, `\n`, and `\\` in `crates/scribe-client/src/smart_selection.rs`
- [X] T042 [US3] Implement interpolated action parameter expansion for `matches[]`, `path`, `user`, and `host` in `crates/scribe-client/src/smart_selection.rs`
- [X] T043 [US3] Implement Copy action execution in `crates/scribe-client/src/main.rs`
- [X] T044 [US3] Implement Open URL action execution with accepted URL scheme handling in `crates/scribe-client/src/main.rs`
- [X] T045 [US3] Implement Open File action execution using focused pane CWD context in `crates/scribe-client/src/main.rs`
- [X] T046 [US3] Implement Send Text action execution through focused pane input handling in `crates/scribe-client/src/main.rs`
- [X] T047 [US3] Implement Run Command action execution in the focused pane in `crates/scribe-client/src/main.rs`
- [X] T048 [US3] Implement Run Coprocess action execution in `crates/scribe-client/src/main.rs`
- [X] T049 [US3] Implement Run Command in Window action execution in `crates/scribe-client/src/main.rs`
- [X] T050 [US3] Add settings UI action editor behavior for action kind, parameter, parameter mode, add, remove, duplicate, and reorder in `crates/scribe-settings/src/assets/settings.js`
- [X] T051 [US3] Ensure selection alone never invokes Smart Selection actions in `crates/scribe-client/src/main.rs`
- [ ] T052 [US3] Verify US3 manually with context-menu action scenarios in `specs/002-smart-selection/quickstart.md`

**Checkpoint**: Smart Selection actions are visible from the terminal context menu and execute only through explicit user choice.

---

## Phase 6: Polish & Cross-Cutting Concerns

**Purpose**: Documentation, validation, and cleanup across the completed feature.

- [X] T053 [P] Document Smart Selection settings behavior in `lat.md/settings.md`
- [X] T054 [P] Document Smart Selection click and matching behavior in `lat.md/client.md`
- [X] T055 [P] Document manual verification coverage in `lat.md/test.md`
- [X] T056 Run `cargo test -p scribe-common smart_selection` for `crates/scribe-common/src/config.rs`
- [X] T057 Run `cargo test -p scribe-settings smart_selection` for `crates/scribe-settings/src/apply.rs`
- [X] T058 Run `cargo test -p scribe-client smart_selection` for `crates/scribe-client/src/smart_selection.rs`
- [X] T059 Run `lat check` for `lat.md/client.md`, `lat.md/settings.md`, and `lat.md/test.md`
- [X] T060 Run `cargo fmt --check` for workspace formatting from `Cargo.toml`
- [X] T061 Review dirty worktree and confirm Smart Selection changes do not overwrite unrelated pre-existing changes in `crates/scribe-client/src/main.rs`

---

## Dependencies & Execution Order

### Phase Dependencies

- **Setup (Phase 1)**: No dependencies; start immediately.
- **Foundational (Phase 2)**: Depends on Setup; blocks all user stories.
- **US1 (Phase 3)**: Depends on Foundational; MVP scope.
- **US2 (Phase 4)**: Depends on Foundational and benefits from US1 for live behavior verification.
- **US3 (Phase 5)**: Depends on Foundational and benefits from US2 for action configuration UI.
- **Polish (Phase 6)**: Depends on completed desired user stories.

### User Story Dependencies

- **User Story 1 (P1)**: Can start after Foundational. No dependency on US2 or US3.
- **User Story 2 (P2)**: Can start after Foundational. Independently verifies persisted config and settings UI; full live verification uses US1.
- **User Story 3 (P3)**: Can start after Foundational. Context-menu execution integrates with matching from US1 and configuration from US2.

### Within Each User Story

- Config/data shapes before client or settings consumers.
- Matcher before click dispatch.
- Settings apply before settings UI save calls.
- Context-menu data model before action execution.
- Manual quickstart verification after each user story phase.

### Parallel Opportunities

- T004 can run in parallel with T001-T003.
- T024-T025 can proceed in parallel with T026-T029 after Foundational, because apply code and markup are separate files.
- T030-T035 and T036 can proceed in parallel after Terminal page markup exists, because JS behavior and CSS styling are separate files.
- T041-T042 can run in parallel with T039-T040 after Foundational, because parameter expansion and context-menu data plumbing are separate files.
- T053-T055 can run in parallel after implementation behavior is stable.

---

## Parallel Example: User Story 2

```text
Task: "T024 [US2] Add terminal.smart_selection and terminal.smart_selection.reset handling in crates/scribe-settings/src/apply.rs"
Task: "T026 [US2] Add a dedicated Smart Selection section after General on the Terminal page in crates/scribe-settings/src/assets/settings.html"
Task: "T036 [US2] Style the Smart Selection section, rule list, editor states, validation states, and empty state in crates/scribe-settings/src/assets/settings.css"
```

## Parallel Example: User Story 3

```text
Task: "T039 [US3] Extend context-menu data to carry Smart Selection rule actions in crates/scribe-client/src/context_menu.rs"
Task: "T041 [US3] Implement legacy action parameter expansion for \\0, \\1-\\9, \\d, \\u, \\h, \\n, and \\\\ in crates/scribe-client/src/smart_selection.rs"
Task: "T042 [US3] Implement interpolated action parameter expansion for matches[], path, user, and host in crates/scribe-client/src/smart_selection.rs"
```

---

## Implementation Strategy

### MVP First (User Story 1 Only)

1. Complete Phase 1: Setup.
2. Complete Phase 2: Foundational.
3. Complete Phase 3: User Story 1.
4. Stop and verify the default Smart Selection examples from `quickstart.md`.
5. Confirm double-click word selection still works when activation is `quad_click`.

### Incremental Delivery

1. Add config and matcher foundation.
2. Deliver US1 default Smart Selection click behavior.
3. Add US2 settings UI and persisted rule management.
4. Add US3 context-menu actions.
5. Finish with `lat.md` updates and verification commands.

### Parallel Team Strategy

1. Complete shared config and matcher foundations together.
2. Split work by file ownership:
   - Client click/matcher owner: `crates/scribe-client/src/main.rs`, `mouse_state.rs`, `selection.rs`, `smart_selection.rs`
   - Settings owner: `crates/scribe-settings/src/apply.rs`, `assets/settings.html`, `assets/settings.js`, `assets/settings.css`
   - Documentation owner: `lat.md/settings.md`, `lat.md/client.md`, `lat.md/test.md`
3. Coordinate before editing `crates/scribe-client/src/main.rs`, since the worktree already has unrelated pending changes in that file.

## Notes

- [P] tasks use different files or independent responsibilities.
- No task should restart the Scribe server.
- Keep Smart Selection actions explicit; selection alone must not run commands.
- Update `lat.md/` only after implementation changes behavior or architecture.

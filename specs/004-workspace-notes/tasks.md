# Tasks: Workspace Notes

**Input**: Design documents from `/specs/004-workspace-notes/`
**Prerequisites**: plan.md, spec.md, research.md, data-model.md, contracts/, quickstart.md

**Tests**: New automated test code is not requested by this feature spec. Tasks
include command verification and manual quickstart verification only.

**Organization**: Tasks are grouped by user story to keep each user-visible
workflow independently deliverable after the shared server/protocol foundation.

**Constitution Gates**: Tasks preserve server/client boundaries, keep notes
server-authoritative, avoid live Scribe server restarts unless explicitly
approved, require story-level verification, and include `lat.md` updates when
runtime behavior changes.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: Can run in parallel after prior phase dependencies are satisfied.
- **[Story]**: User story label for story phases only.
- Every task names exact file paths.

## Phase 1: Setup (Shared Infrastructure)

**Purpose**: Prepare the corrected server-backed work without relying on the
old client-owned task list or persistence model.

- [X] T001 Create the server workspace-notes module entry point in `crates/scribe-server/src/workspace_notes.rs`
- [X] T002 Register the server workspace-notes module from `crates/scribe-server/src/lib.rs`
- [X] T003 [P] Reserve shared protocol type names and message variants in `crates/scribe-common/src/protocol.rs`
- [X] T004 [P] Identify client-local persistence removal points in `crates/scribe-client/src/workspace_notes.rs`

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: Build the authoritative server store, typed IPC, and non-durable
client cache required by every workspace-notes workflow.

**CRITICAL**: No user story work can be considered correct until this phase is
complete.

- [X] T005 Define `WorkspaceNoteEntry`, `WorkspaceNoteDraft`, `WorkspaceNotesCollection`, `WorkspaceNoteStatus`, `ArchiveReason`, and `WorkspaceNotesMutation` in `crates/scribe-common/src/protocol.rs`
- [X] T006 Add `WorkspaceNotesGet`, `WorkspaceNotesMutate`, `WorkspaceNotesSnapshot`, and `WorkspaceNotesChanged` variants in `crates/scribe-common/src/protocol.rs`
- [X] T007 Implement server-owned `WorkspaceNotesStore` data structures and validation rules in `crates/scribe-server/src/workspace_notes.rs`
- [X] T008 Implement versioned TOML load, missing-file fallback, parse-error fallback, and future-version fallback in `crates/scribe-server/src/workspace_notes.rs`
- [X] T009 Implement atomic write-through persistence before ack/broadcast in `crates/scribe-server/src/workspace_notes.rs`
- [X] T010 Implement mutation handlers for save draft, create active note, edit note, archive note, and bulk archived edits in `crates/scribe-server/src/workspace_notes.rs`
- [X] T011 Wire the server notes store into server startup and shared server state in `crates/scribe-server/src/main.rs`
- [X] T012 Add workspace-notes IPC dispatch, error responses, and all-client broadcasts in `crates/scribe-server/src/ipc_server.rs`
- [X] T013 Ensure server handoff/restart uses persisted notes instead of transient handoff payloads in `crates/scribe-server/src/handoff.rs`
- [X] T014 Replace client-owned durable note storage with a non-durable server snapshot cache in `crates/scribe-client/src/workspace_notes.rs`
- [X] T015 Add client IPC request, mutation, snapshot, and broadcast handling helpers in `crates/scribe-client/src/ipc_client.rs`
- [X] T016 Integrate server note snapshots and broadcast events into app state in `crates/scribe-client/src/main.rs`
- [X] T017 Remove client writes to `workspace_notes.toml` while leaving legacy client-local files untouched in `crates/scribe-client/src/workspace_notes.rs`

**Checkpoint**: The server owns all note data, accepted mutations are persisted
before broadcast, and clients can only render cached server state.

---

## Phase 3: User Story 1 - Capture Workspace Notes From the Tab (Priority: P1)

**Goal**: A user can click a workspace tab, type immediately in a centered
modal, use Enter for newlines, and save a workspace-scoped note with
Ctrl+Enter.

**Independent Test**: Click a workspace badge/name, type a two-line note, save
with Ctrl+Enter, and confirm the note appears only for that workspace.

### Implementation for User Story 1

- [X] T018 [US1] Request workspace note snapshots after `SessionList`, reconnect, and workspace creation in `crates/scribe-client/src/ipc_client.rs`
- [X] T019 [US1] Route workspace badge/name clicks to open the notes modal with server-cache lookup in `crates/scribe-client/src/main.rs`
- [X] T020 [US1] Keep the notes modal centered over the clicked workspace pane area in `crates/scribe-client/src/workspace_notes_modal.rs`
- [X] T021 [US1] Focus the modal editor immediately and consume modal keyboard input before PTY routing in `crates/scribe-client/src/main.rs`
- [X] T022 [US1] Render the active-note empty state, multi-line editor, compact controls, and retryable error state in `crates/scribe-client/src/workspace_notes_modal.rs`
- [X] T023 [US1] Send `CreateActiveNote` mutations on Ctrl+Enter and update from `WorkspaceNotesChanged` broadcasts in `crates/scribe-client/src/main.rs`
- [X] T024 [US1] Send debounced `SaveDraft` mutations while typing in `crates/scribe-client/src/main.rs`
- [X] T025 [US1] Force final draft sync on modal close, workspace switch, shutdown, and note save in `crates/scribe-client/src/main.rs`
- [ ] T026 [US1] Manually verify quickstart Scenario 1 and Scenario 5 in `specs/004-workspace-notes/quickstart.md`

**Checkpoint**: User Story 1 is independently functional and is the MVP.

---

## Phase 4: User Story 2 - Manage Active Notes (Priority: P2)

**Goal**: A user can view active workspace notes, edit active notes, and mark
active notes done or removed so they move to archive.

**Independent Test**: Create multiple active notes, edit one, archive another,
and confirm the edited note remains active while the archived note leaves the
active list.

### Implementation for User Story 2

- [X] T027 [US2] Render active note rows from the server-backed cache in `crates/scribe-client/src/workspace_notes_modal.rs`
- [X] T028 [US2] Add hit testing for active note edit, done, remove, save edit, and cancel edit controls in `crates/scribe-client/src/workspace_notes_modal.rs`
- [X] T029 [US2] Route active-note edit saves through `EditNote` mutations in `crates/scribe-client/src/main.rs`
- [X] T030 [US2] Route done and remove controls through `ArchiveNote` mutations with reason metadata in `crates/scribe-client/src/main.rs`
- [X] T031 [US2] Update open modal lists from `WorkspaceNotesChanged` broadcasts without closing the modal in `crates/scribe-client/src/main.rs`
- [X] T032 [US2] Preserve active-note scrolling and compact row layout for long note lists in `crates/scribe-client/src/workspace_notes_modal.rs`
- [ ] T033 [US2] Manually verify quickstart Scenario 2 in `specs/004-workspace-notes/quickstart.md`

**Checkpoint**: User Stories 1 and 2 work independently.

---

## Phase 5: User Story 3 - Review and Edit Archived Notes (Priority: P3)

**Goal**: A user can navigate to archived notes from the modal, edit one
archived note, and use an edit-all archive flow without changing active notes.

**Independent Test**: Archive notes, open archive view, edit a single archived
note, bulk-edit multiple archived notes, and confirm active notes are unchanged.

### Implementation for User Story 3

- [X] T034 [US3] Render archive navigation, archived note rows, and archive empty state from cached server data in `crates/scribe-client/src/workspace_notes_modal.rs`
- [X] T035 [US3] Add hit testing for archive navigation, single archived-note edit, edit-all start, save, and cancel controls in `crates/scribe-client/src/workspace_notes_modal.rs`
- [X] T036 [US3] Route single archived-note edits through `EditNote` mutations without reactivating notes in `crates/scribe-client/src/main.rs`
- [X] T037 [US3] Route edit-all archive saves through `BulkEditArchived` mutations in `crates/scribe-client/src/main.rs`
- [X] T038 [US3] Apply all-or-nothing bulk archived edit validation on the server in `crates/scribe-server/src/workspace_notes.rs`
- [ ] T039 [US3] Manually verify quickstart Scenario 3 in `specs/004-workspace-notes/quickstart.md`

**Checkpoint**: User Stories 1, 2, and 3 work independently.

---

## Phase 6: User Story 4 - Preview Active Notes on Hover (Priority: P4)

**Goal**: A user can hover a workspace tab and instantly see a compact
server-backed active-note preview, then click a preview row to mark it done.

**Independent Test**: Create active notes, hover the workspace tab, move into
the preview, confirm row highlighting, click a row, and confirm archived notes
are excluded on the next hover.

### Implementation for User Story 4

- [X] T040 [US4] Track hover state across workspace badge and preview bounds in `crates/scribe-client/src/main.rs`
- [X] T041 [US4] Build active-note summaries, row bounds, hover targets, truncation, and overflow count from cached server data in `crates/scribe-client/src/workspace_notes_preview.rs`
- [X] T042 [US4] Render compact hover preview chrome with row hover highlighting in `crates/scribe-client/src/workspace_notes_preview.rs`
- [X] T043 [US4] Keep the preview visible while the pointer moves from the tab into preview bounds in `crates/scribe-client/src/main.rs`
- [X] T044 [US4] Route preview row clicks through `ArchiveNote { reason: done }` mutations in `crates/scribe-client/src/main.rs`
- [X] T045 [US4] Suppress preview behind dialogs, context menus, and the notes modal in `crates/scribe-client/src/main.rs`
- [ ] T046 [US4] Manually verify quickstart Scenario 4 and the 100 ms hover target in `specs/004-workspace-notes/quickstart.md`

**Checkpoint**: All user stories are independently functional.

---

## Phase 7: Polish & Cross-Cutting Concerns

**Purpose**: Documentation, validation, performance checks, and cleanup across
all user stories.

- [X] T047 Update workspace notes protocol documentation in `lat.md/protocol.md`
- [X] T048 Update server-owned workspace notes persistence documentation in `lat.md/server.md`
- [X] T049 Update client modal, hover preview, and IPC-backed cache documentation in `lat.md/client.md`
- [X] T050 [P] Update architecture references for server-owned workspace notes in `lat.md/architecture.md`
- [X] T051 [P] Refresh quickstart observations and known constraints in `specs/004-workspace-notes/quickstart.md`
- [X] T052 Run `cargo fmt --all` from repository root `Cargo.toml`
- [X] T053 Run `cargo check -p scribe-common` from repository root `Cargo.toml`
- [X] T054 Run `cargo check -p scribe-server` from repository root `Cargo.toml`
- [X] T055 Run `cargo check -p scribe-client` from repository root `Cargo.toml`
- [X] T056 Run focused clippy verification for changed Rust crates from repository root `Cargo.toml`
- [X] T057 Run `rg "workspace_notes.toml" crates/scribe-client/src` to verify the client no longer writes note state to disk
- [X] T058 Run `lat check` and fix invalid wiki links or code references in `lat.md/`
- [ ] T059 Manually verify quickstart Scenario 6 without restarting the live server in `specs/004-workspace-notes/quickstart.md`
- [ ] T060 Manually verify quickstart Scenario 7 only with a separate development server or explicit restart approval in `specs/004-workspace-notes/quickstart.md`
- [ ] T061 Manually verify quickstart Scenario 8 for ignored legacy client-local note files in `specs/004-workspace-notes/quickstart.md`
- [X] T062 Review task completion against constitution gates and record remaining risks in `specs/004-workspace-notes/tasks.md`

## Current Implementation Review

Command verification passed without restarting the live Scribe server:
`cargo fmt --all`, `cargo check -p scribe-common`, `cargo check -p scribe-server`,
`cargo check -p scribe-client`, `cargo clippy -p scribe-common -p scribe-server -p scribe-client --all-targets -- -D warnings`,
`lat check`, and a client-source scan confirming no `workspace_notes.toml`
references remain in `crates/scribe-client/src`.

Manual quickstart tasks T026, T033, T039, T046, T059, T060, and T061 remain
pending because no separate development client/server session was launched and
the live Scribe server was not restarted.

---

## Dependencies & Execution Order

### Phase Dependencies

- **Setup (Phase 1)**: No dependencies.
- **Foundational (Phase 2)**: Depends on Phase 1 and blocks all user stories.
- **User Story 1 (Phase 3)**: Depends on Phase 2. This is MVP scope.
- **User Story 2 (Phase 4)**: Depends on Phase 2 and uses the capture modal
  from User Story 1 for easiest manual verification.
- **User Story 3 (Phase 5)**: Depends on Phase 2 and archive transitions from
  User Story 2 for full workflow verification.
- **User Story 4 (Phase 6)**: Depends on Phase 2 and active-note data from
  User Story 1 for full workflow verification.
- **Polish (Phase 7)**: Depends on desired user stories being complete.

### User Story Dependencies

- **US1 Capture Workspace Notes**: Required MVP; no dependency on other stories
  after foundation.
- **US2 Manage Active Notes**: Can start after foundation, but final manual
  verification benefits from US1 note creation.
- **US3 Review Archived Notes**: Can start after foundation, but final manual
  verification benefits from US2 archiving.
- **US4 Hover Preview**: Can start after foundation, but final manual
  verification benefits from active notes created by US1 and archived by US2.

### Within Each User Story

- Server/protocol/cache foundation before UI mutation wiring.
- Rendering before hit-test routing when target geometry is produced by render.
- Mutation wiring before manual quickstart verification.
- Manual verification before marking a story complete.

### Parallel Opportunities

- T003 and T004 can run in parallel after T001/T002.
- T007, T008, and T014 can be developed in parallel after protocol shapes are
  drafted, then integrated through T012/T015/T016.
- T020 and T022 can run in parallel after T019.
- T027/T028 and T029/T030 can be split after active-note cache data exists.
- T034/T035 and T038 can run in parallel before archive routing integration.
- T041/T042 can run in parallel with T040, then integrate via T043/T044.
- T047 through T051 can run in parallel after implementation behavior is stable.

---

## Parallel Example: Foundational Work

```text
Task: "Implement server-owned WorkspaceNotesStore data structures and validation rules in crates/scribe-server/src/workspace_notes.rs"
Task: "Replace client-owned durable note storage with a non-durable server snapshot cache in crates/scribe-client/src/workspace_notes.rs"
Task: "Add client IPC request, mutation, snapshot, and broadcast handling helpers in crates/scribe-client/src/ipc_client.rs"
```

## Parallel Example: User Story 4

```text
Task: "Track hover state across workspace badge and preview bounds in crates/scribe-client/src/main.rs"
Task: "Build active-note summaries, row bounds, hover targets, truncation, and overflow count from cached server data in crates/scribe-client/src/workspace_notes_preview.rs"
Task: "Render compact hover preview chrome with row hover highlighting in crates/scribe-client/src/workspace_notes_preview.rs"
```

---

## Implementation Strategy

### MVP First (User Story 1 Only)

1. Complete Phase 1 setup.
2. Complete Phase 2 server/protocol/client-cache foundation.
3. Complete Phase 3 capture workflow.
4. Validate quickstart Scenario 1 and Scenario 5 without restarting the live
   Scribe server.

### Incremental Delivery

1. Foundation: server-owned persistence, typed IPC, client cache.
2. US1: capture notes and preserve drafts.
3. US2: edit and archive active notes.
4. US3: review and edit archived notes.
5. US4: hover preview and preview-row completion.
6. Polish: docs, command verification, manual restart/update checks only with a
   separate dev server or explicit approval.

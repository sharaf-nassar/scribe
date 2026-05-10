# Feature Specification: Releases Page

**Feature Branch**: `001-releases-page`
**Created**: 2026-05-09
**Status**: Draft
**Input**: User description: "Add a new Releases page to the settings window that fetches release notes from GitHub and lets the user navigate between version notes; also fix the incorrect Scribe version shown in the sidebar footer."

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Browse Scribe release notes from inside the app (Priority: P1)

A Scribe user opens the settings window, clicks the new "Releases" entry in the sidebar, sees the most recent Scribe release's notes already rendered in the panel, and can move to other versions either by picking one from a version selector that lists recent releases or by clicking dedicated Newer / Older navigation buttons that step one release in the newer or older direction at a time, reading each release's notes rendered as readable text in the same panel without leaving the app.

**Why this priority**: This is the primary feature the user requested. Without it, the rest of the work has no destination. It also delivers immediate user value the first time it ships — a user who installs Scribe can finally answer "what changed?" without leaving the app.

**Independent Test**: Open settings, click "Releases" in the sidebar, confirm the most recent release's notes are rendered, open the version picker and select an older release, confirm its notes are displayed; then use the Newer / Older navigation buttons to step through adjacent versions and confirm both controls drive the same content area and stay in sync.

**Acceptance Scenarios**:

1. **Given** the settings window is open and the user is online, **When** the user clicks the "Releases" entry in the sidebar, **Then** the panel shows the most recent release's notes rendered in the content area, with the version picker pre-set to that release.
2. **Given** the Releases page is open and showing a release, **When** the user opens the version picker and selects a different release, **Then** the content area updates to show that release's notes within 200ms of the selection and the picker reflects the new selection.
3. **Given** the Releases page is showing any release that is neither the newest nor the oldest in the list, **When** the user clicks the "Older" navigation button, **Then** the content area and the version picker both move one release in the older direction; clicking "Newer" moves one release in the newer direction.
4. **Given** the Releases page is showing the newest release, **When** the user looks at the navigation buttons, **Then** the "Newer" button is visibly disabled and clicking it has no effect; the same applies to the "Older" button when showing the oldest release in the list.
5. **Given** a release whose notes contain markdown formatting (headings, lists, links, inline code, code blocks), **When** that release is selected, **Then** the content area renders the markdown as formatted text, not as raw markup.
6. **Given** a release that is marked as a pre-release on GitHub, **When** it appears in the version picker, **Then** it is visually distinguishable from stable releases (e.g. a "Pre-release" badge or label) so the user can tell them apart at a glance.

---

### User Story 2 - See the accurate running Scribe version in the sidebar footer (Priority: P2)

A Scribe user glances at the settings sidebar footer and sees the actual version of Scribe they are running, not a placeholder, so they can quickly answer "what version am I on?" — for example before filing a bug report or comparing against the Releases page they just opened.

**Why this priority**: This is a small but visible correctness bug the user explicitly called out. It is independent of the Releases page itself (the sidebar footer is shared chrome) and worth shipping on its own even if the Releases page slips. Users who never open Releases still benefit.

**Independent Test**: Build Scribe at a known version, launch the settings window, and confirm the footer displays exactly that version. Repeat after a version bump and confirm the displayed value updates without any manual edit to the settings UI source.

**Acceptance Scenarios**:

1. **Given** Scribe is built at any version X, **When** the user opens the settings window, **Then** the sidebar footer displays the running Scribe version as "Scribe vX" (or the project's standard version-string format) with no placeholder or stale value.
2. **Given** the Scribe version is bumped between two builds, **When** the user opens the settings window in each build, **Then** each build's footer shows that build's own version string with no source-code edit required to keep them in sync.
3. **Given** the running version cannot be determined for any reason, **When** the settings window opens, **Then** the footer either shows a clearly non-misleading fallback (e.g. just "Scribe") or omits the version entirely, but never displays a stale hardcoded number.

---

### User Story 3 - Use the Releases page when GitHub is slow, rate-limited, or unreachable (Priority: P3)

A Scribe user on a flaky network, behind a corporate firewall, or hit by GitHub's unauthenticated rate limit opens the Releases page and still sees something useful — either previously fetched release data or a clear, non-blocking explanation — instead of an indefinite spinner or a broken panel.

**Why this priority**: Resilience matters but is not the core of the feature. A user who is online almost never sees these states. We want the feature to degrade gracefully rather than fail loudly, but we do not want to delay shipping the core browsing experience for it.

**Independent Test**: With the network disconnected, open the Releases page; confirm the panel shows either the most recently cached release data with a clearly visible "may be stale" indicator, or a non-blocking error state with an explanation and a retry control. Confirm the rest of the settings window remains usable.

**Acceptance Scenarios**:

1. **Given** the user has previously loaded the Releases page successfully, **When** they open it again while offline, **Then** the previously fetched release list and notes are shown along with a visible indicator that the data may be stale, and a way to retry.
2. **Given** the user has never loaded the Releases page and is offline, **When** they open it, **Then** the panel shows a clear, plain-language message explaining the page could not load and offers a retry, without freezing the rest of the settings window.
3. **Given** the GitHub API returns a rate-limit error, **When** the user opens the Releases page, **Then** the panel shows a message that distinguishes rate-limiting from a generic failure and indicates that the data will refresh later automatically.
4. **Given** the GitHub API returns a malformed response or an unexpected schema, **When** the user opens the Releases page, **Then** the panel surfaces a generic "could not load releases" message and the rest of the settings window keeps functioning.

---

### Edge Cases

- The repository has zero published releases (e.g. early in the project's life): the panel must show a friendly empty state, not an error.
- A release has an empty body (no release notes written): the content area must show a clear "No release notes provided for this version" message instead of a blank canvas.
- A release body is extremely long (tens of thousands of characters): the content area must remain scrollable and responsive; the list must not block on rendering.
- A release body contains links: links must open in the user's external browser, not navigate the embedded webview off the settings page.
- A release body references images: images must either render correctly or fail gracefully without breaking the rest of the rendered notes.
- The user opens the Releases page, switches to another sidebar entry, and comes back: the previously selected version stays selected, with no extra network round-trip if data is still fresh.
- The user is on the Beta update channel: the Releases page behavior with respect to pre-releases is consistent with the user's expectations (see Assumptions).
- The user has the settings window open while a Scribe self-update completes in the background: opening the Releases page after the bump still works and the sidebar footer reflects the new running version (subject to whether the settings process restarts).

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The settings sidebar MUST include a "Releases" entry alongside the existing entries (Appearance, Colors, AI, Terminal, Keybindings, Workspaces, Updates, Notifications).
- **FR-002**: Selecting the "Releases" entry MUST display, in the settings content area, the notes for one Scribe release at a time, with controls to choose which release is shown drawn from the project's GitHub releases.
- **FR-003**: The set of available releases MUST be ordered with the newest release first throughout every control that exposes them (version picker, navigation buttons).
- **FR-004**: The version picker MUST show, for each available release, at minimum the version (e.g. tag name) and the release date in a human-readable form.
- **FR-005**: Pre-releases MUST be visually distinguishable from stable releases in every control that exposes them (e.g. with a "Pre-release" badge or label in the version picker).
- **FR-006**: Draft releases MUST NOT appear in any control or in the rendered content.
- **FR-007**: When the user selects a release through any control, the content area MUST display that release's notes rendered from markdown to formatted text in a readable typography consistent with the rest of the settings panels.
- **FR-008**: The first time the page is opened, the most recent available release MUST be selected and rendered automatically.
- **FR-009**: Switching between releases MUST NOT trigger a new GitHub network request if the data was already fetched in the same session unless the user explicitly refreshes.
- **FR-010**: All GitHub network access for this feature MUST go through the same in-process component that already performs Scribe's release lookups today, rather than a new direct HTTP path opened from the embedded webview.
- **FR-011**: The system MUST NOT block opening the Releases page on the GitHub round-trip; the panel MUST render immediately with a clear loading state and update once data arrives.
- **FR-012**: When GitHub is unreachable, slow beyond a sensible timeout, or returns an error, the panel MUST show a non-blocking failure state with a plain-language explanation and a way to retry, without freezing the rest of the settings window.
- **FR-013**: When previously fetched release data is available and the latest fetch fails, the panel MUST show the previous data with a visible "may be stale" indicator instead of replacing it with an error.
- **FR-014**: Links inside rendered release notes MUST open in the user's external default browser, not inside the settings webview.
- **FR-015**: The settings sidebar footer MUST display the running Scribe version derived from the actual build (not a hardcoded literal) such that bumping the project's published version updates the footer with no source edit to the settings UI.
- **FR-016**: If the running version cannot be determined, the footer MUST NOT display a misleading or stale value; it MUST instead omit the version or show a clearly non-version fallback.
- **FR-017**: The Releases page MUST follow the existing settings visual style (sharp-cornered, dark theme, no glassmorphism, no oversized rounded cards) and MUST NOT introduce a JavaScript framework or third-party UI library to the settings webview.
- **FR-018**: The number of releases shown MUST be bounded to the 30 most recent non-draft releases (matching the GitHub releases API default page size; see research R3). Pagination beyond this window is out of scope for v1.
- **FR-019**: The Releases page MUST provide dedicated Newer and Older navigation controls that step the displayed release one position in the newer or older direction respectively. The Newer control MUST be visibly disabled while the newest release is shown; the Older control MUST be visibly disabled while the oldest available release is shown. Clicking a disabled control MUST have no effect.
- **FR-020**: The version picker and the Newer / Older navigation controls MUST stay in sync at all times — selecting a release through one control MUST update the displayed selection in the other.

### Key Entities *(include if feature involves data)*

- **Release**: A single published version of Scribe. Attributes relevant to this feature: a version identifier (tag name), a human-readable name, a publish date, the release notes body in markdown, a flag indicating whether it is a pre-release, a flag indicating whether it is a draft, and a canonical web URL on GitHub. Relationships: ordered chronologically; one release is currently selected at a time in the UI.
- **Release Cache**: The most recently fetched list of releases held by the running Scribe session, used to back the Releases page when GitHub is slow or unreachable. Attributes: timestamp of the last successful fetch, the list of Release objects, and a freshness indicator. Relationships: the cache is per running Scribe session; it does not need to persist across restarts for this feature.
- **Running Version**: The version string of the Scribe build the user is currently running. It is the source of truth displayed in the sidebar footer and is what the Releases page can highlight (e.g. "you are here") if it appears in the list.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A first-time user can open the settings window and reach a rendered release-notes view (latest version) within 5 seconds of clicking "Releases" on a typical broadband connection, including the network fetch.
- **SC-002**: Switching between two releases that have already been fetched in the same session updates the displayed notes in under 200 ms of the click, with no additional network request.
- **SC-003**: A user offline or behind a firewall that blocks GitHub never sees a frozen settings window when opening the Releases page; the panel reaches a non-blocking failure or stale-data state within 5 seconds.
- **SC-004**: The version string in the sidebar footer matches the running Scribe build's published version exactly in 100% of builds for at least three consecutive version bumps, with no manual edit to the settings UI between bumps.
- **SC-005**: A reviewer comparing the Releases page side-by-side with the existing settings panels confirms it uses the same visual style (typography, spacing, sharp corners, dark theme) without any additional review iteration on look-and-feel.

## Assumptions

- The new "Releases" entry is added alongside the existing "Updates" entry in the sidebar; the existing "Updates" page is not removed or merged. The two pages serve different purposes: "Updates" controls auto-update behavior (enabled, channel, interval), while "Releases" is an informational changelog browser.
- The Releases page shows release history regardless of the user's update channel setting (Stable vs. Beta). Pre-releases are always listed, marked clearly. The update channel setting continues to govern only the auto-updater, not what the user can read about.
- The "running Scribe version" displayed in the sidebar footer is the version of the Scribe product as a whole, derived from the build. The settings binary and the server are released together in lockstep, so either one's compiled version is acceptable as the source of truth.
- GitHub is queried using the same unauthenticated, public access path that Scribe's existing updater uses today; no new authentication mechanism, token storage, or user-supplied credential is introduced.
- A bounded list of the 30 most recent non-draft releases (per FR-018) is sufficient for this feature; deep historical archaeology of every release ever cut is out of scope for v1 and can be revisited later.
- Markdown rendering targets the markdown features GitHub release notes commonly use (headings, lists, links, inline code, code blocks, basic emphasis). Exotic GitHub-specific extensions (e.g. task list checkboxes interactivity, mentions, issue references that turn into live links) are nice-to-have but not required for v1.
- This feature does not change any existing behavior of the auto-updater, the "Updates" page, or the update notification flow. Reuse of the existing GitHub-fetching path on the server is purely additive.
- Persisting the release cache to disk across Scribe restarts is out of scope for v1; the cache is a same-session affordance.

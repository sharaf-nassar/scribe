# Feature Specification: Smart Selection

**Feature Branch**: `002-smart-selection`  
**Created**: 2026-05-12  
**Status**: Draft  
**Input**: User description: "Add the same smart selection feature as iTerm2. It should be its own section on the Terminal settings page. It should allow configuring smart selection as either double click or quad click and have the same config options as iTerm2, without profiles."

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Select Semantic Text Quickly (Priority: P1)

A terminal user wants Smart Selection to pick the full semantic object under the cursor, such as a URL, path, quoted string, include path, email address, selector, or whitespace-bounded word, without manually dragging across the text.

**Why this priority**: This is the core user value. Users should get iTerm2-like semantic selection before any advanced rule editing or actions matter.

**Independent Test**: Configure Smart Selection for quad click, display each default recognized text kind in the terminal, quad-click inside each object, and confirm the complete object is selected while unrelated surrounding text is not.

**Acceptance Scenarios**:

1. **Given** Smart Selection is enabled for quad click and the terminal shows `https://example.com/path?q=1`, **When** the user quad-clicks anywhere inside the URL, **Then** the full URL is selected.
2. **Given** Smart Selection is enabled for quad click and the terminal shows `/tmp/build/output.log`, **When** the user quad-clicks any character in the path, **Then** the full path is selected.
3. **Given** Smart Selection is enabled for quad click and the terminal shows `"quoted value with spaces"`, **When** the user quad-clicks inside the quoted text, **Then** the full quoted string including delimiters is selected.
4. **Given** no Smart Selection rule matches the clicked character, **When** the user invokes Smart Selection, **Then** the existing selection state is not replaced by a misleading partial match.

---

### User Story 2 - Configure Smart Selection Rules (Priority: P2)

A user wants a dedicated Terminal settings section where they can review, add, edit, remove, restore, and validate Smart Selection rules globally, rather than configuring them per profile.

**Why this priority**: Rule editing makes the feature match iTerm2's customizable behavior and lets users adapt selection to project-specific output.

**Independent Test**: Open Terminal settings, edit a rule's regular expression and precision, add a new rule, save, reopen settings, and confirm the saved rules drive Smart Selection results in all terminal panes.

**Acceptance Scenarios**:

1. **Given** the Terminal settings page is open, **When** the user opens the Smart Selection section, **Then** they can see the activation gesture and the full rule list in one place.
2. **Given** the user adds a rule with a valid regular expression and precision, **When** they save settings, **Then** the rule is available in every terminal pane without choosing a profile.
3. **Given** the user enters an invalid regular expression, **When** they try to save or test it, **Then** the settings page identifies the invalid rule and prevents a broken configuration from silently taking effect.
4. **Given** the user changes a rule's precision, **When** two rules match the same cursor location, **Then** the rule with the stronger precision wins over lower precision matches unless the documented scoring rules make another match better.

---

### User Story 3 - Invoke Rule Actions (Priority: P3)

A user wants matching Smart Selection rules to expose context-menu actions like opening a file, opening a URL, running a command, sending text, or copying the match, using the configured action parameters.

**Why this priority**: Actions complete the iTerm2-like feature set, but semantic selection and rule editing remain valuable without them.

**Independent Test**: Configure actions on a Smart Selection rule, right-click matching text in the terminal, and confirm the actions appear in the context menu and execute only after explicit user selection.

**Acceptance Scenarios**:

1. **Given** a Smart Selection rule has an Open URL action and the terminal contains matching text, **When** the user opens the context menu over that text, **Then** the Open URL action appears for that match.
2. **Given** a Smart Selection rule has multiple actions, **When** the user opens the context menu over matching text, **Then** all configured actions for that rule are available in their configured order.
3. **Given** an action parameter contains match placeholders, **When** the user invokes the action, **Then** placeholders are replaced with the full match, captured groups, and available session context before the action runs.
4. **Given** an action can run a command or send text, **When** Smart Selection merely creates a selection, **Then** that action is not executed until the user explicitly chooses it.

### Edge Cases

- Multiple rules match the cursor location with different precision levels.
- Multiple rules match the cursor location with the same precision level but different match lengths.
- A rule matches across soft-wrapped terminal rows.
- A rule matches text split by a hard newline.
- A configured activation gesture conflicts with normal word or line selection.
- Invalid regular expressions, empty rules, and action parameters with missing capture groups.
- Remote host, user name, or current directory context is unavailable for an action parameter.
- Smart Selection is invoked while a terminal application has mouse reporting enabled.
- Rules or actions are edited while terminal panes are already open.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The Terminal settings page MUST include a dedicated Smart Selection section separate from general selection and clipboard settings.
- **FR-002**: Users MUST be able to choose the Smart Selection activation gesture as either double click or quad click.
- **FR-003**: When Smart Selection is set to double click, double click MUST perform Smart Selection instead of ordinary word selection.
- **FR-004**: When Smart Selection is set to quad click, double click MUST continue to perform ordinary word selection and quad click MUST perform Smart Selection.
- **FR-005**: The system MUST provide default Smart Selection rules that recognize whitespace-bounded words, `namespace::identifier` pairs, filesystem paths, quoted strings, Java/Python-style include paths, URIs using `mailto`, `http`, `https`, `ssh`, or `telnet`, Objective-C-style selectors, and email addresses.
- **FR-006**: Users MUST be able to add, edit, remove, duplicate, reorder for presentation, enable, disable, and restore Smart Selection rules from the Smart Selection settings section.
- **FR-007**: Each Smart Selection rule MUST include a regular expression and a precision value of Very Low, Low, Normal, High, or Very High.
- **FR-008**: Smart Selection MUST consider only matches that include the character under the cursor.
- **FR-009**: For each rule, Smart Selection MUST identify the longest matching string that includes the cursor character before comparing it with other rule candidates.
- **FR-010**: Smart Selection MUST choose the candidate from the highest precision class that has any match; when candidates share that precision, the longest candidate MUST be selected.
- **FR-011**: Users MUST be able to validate a rule against sample text in the settings page before relying on it in terminal content.
- **FR-012**: Invalid regular expressions MUST be reported clearly and MUST NOT break terminal selection for valid rules.
- **FR-013**: Rules MUST apply globally across terminal panes and windows, with no profile-specific configuration.
- **FR-014**: Rule changes saved in settings MUST affect already-open terminal panes without requiring a new session.
- **FR-015**: Users MUST be able to configure zero or more actions on each Smart Selection rule.
- **FR-016**: Supported Smart Selection action types MUST include Open File, Open URL, Run Command, Run Coprocess, Send Text, Run Command in Window, and Copy.
- **FR-017**: Each action MUST have a parameter field whose meaning depends on the selected action type.
- **FR-018**: Action parameters MUST support legacy placeholders for the full match, capture groups 1-9, current directory, user name, host name, newline, and literal backslash.
- **FR-019**: Action parameters MUST also support an interpolated-string mode with access to the full match and capture groups.
- **FR-020**: When the user opens the terminal context menu over text matched by a Smart Selection rule, the context menu MUST include that rule's configured actions.
- **FR-021**: If multiple matching rules have actions at a context-menu location, the context menu MUST include actions from every matching rule in a predictable order.
- **FR-022**: Smart Selection MUST NOT execute configured actions merely because text was selected.
- **FR-023**: Smart Selection MUST preserve existing copy-on-select behavior after it creates a selection.
- **FR-024**: Smart Selection MUST preserve existing mouse-reporting expectations, including the existing modifier behavior used to bypass application mouse handling for terminal selection.
- **FR-025**: Users MUST be able to reset Smart Selection settings to the default rule set and default activation gesture.

### Key Entities *(include if feature involves data)*

- **Smart Selection Settings**: Global terminal preference group containing activation gesture, rule list, and reset state.
- **Smart Selection Rule**: A semantic-selection definition with enabled state, display name, regular expression, precision level, and action list.
- **Smart Selection Candidate**: A possible match under the cursor with matched text, matched range, precision level, and capture groups.
- **Smart Selection Action**: A user-invoked operation associated with a rule, including action type, parameter, and parameter interpolation mode.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: Users can configure the activation gesture and save a valid custom Smart Selection rule in under 2 minutes.
- **SC-002**: At least 95% of default recognized examples select the complete intended object on the first Smart Selection attempt.
- **SC-003**: Smart Selection returns a visible selection within 100 ms for typical visible terminal content.
- **SC-004**: Invalid rules are caught before use in 100% of tested settings-save and rule-test flows.
- **SC-005**: Existing double-click word selection remains unchanged when Smart Selection is configured for quad click.
- **SC-006**: Context-menu actions are discoverable and invokable for every action type supported by the feature.

## Assumptions

- Smart Selection settings are global because Scribe does not expose iTerm2-style profiles for this request.
- Quad click is the default activation gesture, matching iTerm2's documented default.
- Existing triple-click line selection remains line selection; the requested configurable gestures are double click and quad click only.
- Existing URL and file opening behavior can be reused where it already matches the configured action semantics.
- Command-like actions require explicit user invocation and must not run automatically during selection.
- If host, user, or directory context is unavailable for an action parameter, the action still remains visible but the missing value is substituted predictably and documented.

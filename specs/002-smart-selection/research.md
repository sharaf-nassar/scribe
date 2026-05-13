# Research: Smart Selection

## Decision: Match iTerm2's user-facing Smart Selection model

Use iTerm2's visible concepts: a Smart Selection gesture, editable regex rules, five precision levels, default semantic recognizers, and optional actions exposed from the context menu.

**Rationale**: The requested feature explicitly names iTerm2 and asks for the same configuration options without profiles. iTerm2 documents quad click as the default Smart Selection gesture, default recognizers for URLs, paths, quoted strings, include paths, email addresses, selectors, namespace identifiers, and words, and a rule model based on regular expressions plus precision.

**Alternatives considered**:
- Only improve double-click word boundaries. Rejected because it does not provide configurable semantic rules or actions.
- Reuse existing URL/path hover detection. Rejected because it only covers a subset of iTerm2 Smart Selection.

## Decision: Use the existing Rust `regex` crate for rule matching

Compile enabled Smart Selection rules with the existing workspace `regex` dependency and cache compiled rules for click-time matching.

**Rationale**: The workspace already depends on `regex = "1"`. Current docs for `regex` 1.12.3 state it supports Unicode-aware matching, capture groups, match iteration, and returns byte offsets that are valid UTF-8 boundaries. It also provides worst-case `O(m * n)` search behavior, which matters because users can configure patterns and terminal content is untrusted. The docs recommend compiling each regex once and reusing it because compilation is expensive.

**Compatibility note**: iTerm2 documents ICU regular expressions. Rust `regex` is not full ICU-compatible and intentionally omits features such as look-around and backreferences. The plan preserves iTerm2's configuration shape and behavior model while using Rust regex syntax for safety and packaging simplicity.

**Alternatives considered**:
- Add an ICU-compatible engine. Rejected for this plan because it would introduce a new dependency surface and packaging risk before there is evidence users need ICU-only constructs.
- Add a backtracking regex engine for closer syntax compatibility. Rejected because user-configured patterns could create unpredictable click-time latency.
- Hand-code all default recognizers. Rejected because custom rules are a primary requirement.

Sources:
- iTerm2 Smart Selection docs: https://iterm2.com/documentation-smart-selection.html
- iTerm2 General Preferences docs: https://iterm2.com/3.4/documentation-preferences-general.html
- Rust regex docs: https://docs.rs/regex/latest/regex/

## Decision: Store settings globally under terminal configuration

Add Smart Selection configuration to the shared terminal config rather than introducing profiles or per-pane state.

**Rationale**: The feature explicitly excludes profiles. Existing terminal settings such as copy-on-select and scrollback are global and live under shared config consumed by both the client and settings process.

**Alternatives considered**:
- Per-window or per-pane configuration. Rejected because it adds scope not requested by the user.
- Separate standalone settings file. Rejected because existing terminal settings already persist through `ScribeConfig`.

## Decision: Extend click classification to quad click

Preserve existing single/double/triple behavior and add a fourth click kind that maps to Smart Selection when the activation gesture is `quad_click`.

**Rationale**: Existing click classification already tracks press time and position. Adding quad click keeps the behavior local to mouse-selection dispatch and avoids changing drag or rendering semantics.

**Alternatives considered**:
- Treat quad click as repeated triple click. Rejected because users need a distinct gesture for Smart Selection.
- Add a keyboard modifier gesture instead. Rejected because the spec asks for double click or quad click.

## Decision: Use logical visible text with grid coordinate mapping

Build a logical text buffer from visible terminal cells, preserving a map from each character back to absolute grid row and column, and use it for Smart Selection matching.

**Rationale**: Existing word selection and URL detection already rely on absolute grid coordinates and WRAPLINE behavior. Smart Selection needs the same ability to select across soft-wrapped rows and to map regex byte offsets back to terminal cells.

**Alternatives considered**:
- Match only one screen row. Rejected because the spec includes soft-wrapped and hard-newline edge cases.
- Match the full scrollback history on each click. Rejected because it adds unnecessary latency and the Smart Selection action is anchored to visible cursor context.

## Decision: Context-menu actions are explicit only

Selecting text with Smart Selection never runs configured actions. Right-click over matching text shows configured actions, and choosing an action executes it.

**Rationale**: The spec requires command-like actions not to execute during selection. This matches Scribe's current context-menu pattern for explicit Open URL/Open File actions and avoids surprising command execution.

**Alternatives considered**:
- Run the first action on Ctrl-click/Cmd-click immediately. Deferred because the spec only requires context-menu actions and explicit invocation.
- Disable command-like actions. Rejected because iTerm2's action model includes them and the spec asks for the same options.

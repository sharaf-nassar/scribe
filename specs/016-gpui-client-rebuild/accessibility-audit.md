# GPUI chrome accessibility audit

Audited 2026-07-28 at `65b14b6597971f60ea12d62379d0ccaefa0c637d`.
This is a source audit of the shipped GPUI client chrome and settings surface;
it records code-verifiable findings, the applicable remediation bead, and the
manual checks each remediation must complete.

## Scope and method

Scope is the terminal window's custom titlebar, tab strip, pane and window
status chrome, and the standalone settings window. The terminal grid itself
and non-GPUI legacy client are out of scope.

The audit traced every interactive `on_click` / mouse listener and every
`FocusHandle` / keyboard listener in `crates/scribe-client/src`. It also
searched the complete client source for GPUI `a11y_*` and `accesskit` use, then
checked the pinned GPUI accessibility guide. GPUI only exports a node when it
has both a stable global ID and an accessibility role; `on_click` supplies a
Click action only after the node is exposed.

No live server was started or restarted. The results do not rely on the
previously installed `scribe-dev` binary: it is not a reliable substitute for
the exact audited source revision or for an enabled screen reader.

## Findings

| ID | Surface | Finding | User impact | Priority | Remediation |
| --- | --- | --- | --- | --- | --- |
| A11Y-01 | Titlebar and tabs | Tabs, tab-close targets, equalize/settings icons, and custom window controls were mouse-only. `TitlebarView` tracked only its root focus and had no key listener or child focus order. | Keyboard-only users could not operate custom chrome or discover its focused control. | P1 | `scribe-38e.108` |
| A11Y-02 | Titlebar and tabs | No titlebar element sets an AccessKit role, accessible name, selected state, or non-pointer action. Icon glyphs (`⚙`, `⊞`, `N`, `×`) therefore do not convey purpose; tabs do not expose their title or active state. | Screen readers and voice control cannot identify or invoke terminal-window chrome. | P1 | `scribe-38e.109` |
| A11Y-03 | Settings operation | Settings has one root `FocusHandle`; its sidebar, toggles, choices, steppers, actions, and trust mutation controls are generic click targets with no child focus or key handling. | Keyboard-only users cannot navigate settings, change a value, or reach overflow content reliably. | P1 | `scribe-38e.110` |
| A11Y-04 | Settings semantics | Settings rows render generic divs. Labels are not programmatically associated with controls; current value, selected page, toggle state, and preflight/trust feedback are not exposed as accessible state or live feedback. | Screen-reader users cannot understand settings structure, state, or action results. | P1 | `scribe-38e.111` |
| A11Y-05 | Status chrome | The status bar and prompt/status chrome were anonymous text/div trees. Its update CTA had only a pointer click handler, and connection/error/update state had no concise accessible status channel. | Assistive technology misses important terminal-window state and cannot invoke the update CTA. | P2 | `scribe-38e.112` |

Every defect found by this audit has one remediation bead. The P1 work removes
the complete keyboard and semantic blockers before the P2 status-announcement
polish; the P2 task remains required before the GPUI chrome accessibility gate
can be called complete.

## Evidence

- `crates/scribe-client/src/titlebar.rs`: the root uses `track_focus`, while
  `render_tab`, `render_tab_close`, `render_icon_button`, and
  `render_window_control` used mouse/click
  listeners only. No `on_key_down`, role, label, or accessibility action is
  present.
- `crates/scribe-client/src/settings/window.rs`: the root uses `track_focus`;
  `render_nav`, `render_value_widget`, `render_stepper`, and `trust_row` build
  clickable `div`s with no keyboard or accessibility semantics. The `pill`
  helper repeats the same generic pattern for most controls.
- `crates/scribe-client/src/status_bar.rs`: `center_cta` conditionally adds an
  `on_click` handler, while `render` and `span_row` build anonymous text
  containers. The same source-wide search found no client `a11y_*` or
  `accesskit` call.
- The pinned GPUI accessibility guide establishes the missing prerequisites:
  stable IDs plus roles create exposed nodes; `on_click` only registers an
  accessible Click action for such a node.

## Remediation verification gate

Each remediation must be verified with a real enabled screen reader on Linux
(Orca/AT-SPI), macOS (VoiceOver), or Windows (Narrator/UIA), plus keyboard-only
operation. The final closure check must cover this order:

1. Enter the terminal window, move through custom titlebar controls, identify
   every tab and its selected state, activate settings/equalize/close, and
   return focus to a predictable tab or terminal target.
2. Open settings, traverse sidebar and content in a stable order, operate
   toggle/choice/stepper/action controls with the keyboard, and confirm changed
   values and async trust/preflight outcomes are announced once.
3. Trigger connection, error, prompt, and update states; confirm the status
   chrome announces useful state without reading decorative spans separately,
   and operate the update CTA without a pointer.
4. Capture the GPUI accessibility tree in a debug build and assert unique,
   stable IDs plus correct roles, names, selected/checked/value state, and
   supported actions for each exposed control.

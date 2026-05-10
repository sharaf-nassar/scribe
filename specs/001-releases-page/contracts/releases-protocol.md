# Contract: Releases Protocol

**Feature**: 001-releases-page
**Date**: 2026-05-09
**Spec**: [../spec.md](../spec.md) | **Plan**: [../plan.md](../plan.md) | **Data model**: [../data-model.md](../data-model.md)

This document is the source of truth for the wire shapes the feature introduces. Two interface contracts are defined: the server↔settings IPC (msgpack over Unix socket, the existing channel) and the webview↔settings-host IPC (JSON `postMessage`, the existing channel).

## 1. Server ↔ Settings: `ClientMessage::ListReleases` / `ServerMessage::ReleaseList`

The settings binary issues a one-shot synchronous request over the existing Unix socket using the same `write_frame` / `read_frame` helpers in `crates/scribe-settings/src/server_action.rs`. The framing (4-byte big-endian length prefix + msgpack-named payload) is unchanged.

### 1.1 Request

`ClientMessage::ListReleases`

- No payload.
- Sent exactly once per call to `request_release_list(timeout)`.
- Variant tag follows the existing `#[serde(tag = "type")]` convention on `ClientMessage` (internal tagging, PascalCase variant names — verified against `crates/scribe-common/src/protocol.rs` at implementation time).

Wire example (msgpack, named):

```text
{ "type": "ListReleases" }
```

### 1.2 Response

`ServerMessage::ReleaseList { state: ReleaseListResultState }`

`ServerMessage` uses `#[serde(tag = "type")]` (internal tagging, PascalCase variants), so the `type` discriminator and the message body fields are siblings — there is no nested `data` envelope.

`ReleaseListResultState` has **no** serde attribute and therefore uses the default external-tag representation (`{ "VariantName": { ...payload } }`), matching the existing `UpdateCheckResultState` convention. Variants:

```text
ReleaseListResultState::Fresh { releases }
ReleaseListResultState::Stale { releases, reason }
ReleaseListResultState::Failed { reason }
```

Each `releases` element carries:

```text
Release {
  version       : string         // e.g. "0.4.2"
  name          : string | null  // optional human title
  published_at  : string         // ISO-8601, verbatim from GitHub
  body_html     : string         // sanitized HTML, ready for innerHTML
  prerelease    : bool
  html_url      : string         // canonical GitHub URL for "View on GitHub"
}
```

Wire examples (msgpack, named, using JSON-equivalent rendering for readability):

```text
// Fresh — cache hit within TTL, or fresh fetch just succeeded
{
  "type": "ReleaseList",
  "state": {
    "Fresh": {
      "releases": [
        {
          "version": "0.4.2",
          "name": "0.4.2 — Releases page",
          "published_at": "2026-05-09T10:00:00Z",
          "body_html": "<h2>Highlights</h2>\n<ul><li>…</li></ul>",
          "prerelease": false,
          "html_url": "https://github.com/sharaf-nassar/scribe/releases/tag/v0.4.2"
        }
      ]
    }
  }
}

// Stale — refresh failed but cache still has data
{
  "type": "ReleaseList",
  "state": {
    "Stale": {
      "releases": [ /* … */ ],
      "reason": "GitHub unreachable"
    }
  }
}

// Failed — no cache to fall back to
{
  "type": "ReleaseList",
  "state": {
    "Failed": {
      "reason": "GitHub rate limit reached, retry after 12 minutes"
    }
  }
}
```

### 1.3 Error and timeout behavior

- The settings binary's `request_release_list(timeout)` mirrors the existing `request_update_check`: any transport or protocol error is converted to `ReleaseListResultState::Failed { reason }` so the UI always has a single shape to render.
- If the server crashes the connection mid-frame, the helper produces `Failed { reason: "<transport error>" }` and the panel shows the failure state.
- The client never partially renders a `Fresh` payload; either the whole list arrives and is rendered, or the panel remains in the loading or error state.

### 1.4 Backwards compatibility

- The existing `ClientMessage` and `ServerMessage` variants are untouched. The added variants are new tag values; older builds of either side will see them as "unknown variant" in serde and reject the frame, which is acceptable because `scribe-server` and `scribe-settings` are released together.
- No HANDOFF version bump is needed: the handoff state schema is independent of these messages (the existing protocol additions in `crates/scribe-server/src/handoff.rs` are about server-state hot-reload, not client/server message variants).

## 2. Webview ↔ Settings host: `postMessage` JSON IPC

The webview already uses `window.ipc.postMessage(JSON.stringify(message))` for `setting_changed` and host-action messages. The Releases page adds two new message types and one host-to-webview broadcast.

### 2.1 Webview → Host

`request_releases`

- Payload: `{ "type": "request_releases" }`.
- Sent the first time the user activates the Releases tab in a session; not sent on subsequent activations if `releases` is already populated in JS state (see data-model "Settings webview state").
- Resent only on explicit user action: the panel's "Retry" or "Refresh" control.

`open_external_url`

- Payload: `{ "type": "open_external_url", "url": "<absolute http(s) URL>" }`.
- Sent when the user clicks any anchor inside the rendered release-notes HTML, including the panel header's "View on GitHub" link.
- The webview's link-handler MUST call `event.preventDefault()` before posting the message so the embedded webview never navigates off the settings page.
- The host MUST validate `url` starts with `http://` or `https://` before invoking the platform opener, and ignore any other scheme.

### 2.2 Host → Webview

The host responds to `request_releases` by calling `request_release_list(timeout)` synchronously, then injecting the result into the webview via the existing `evaluate_script` mechanism that the settings binary already uses for state delivery. The injected script is a single function call:

```js
window.SCRIBE_ON_RELEASE_LIST({
  state: "fresh" | "stale" | "failed",
  releases: [ /* Release objects from §1.2, only present for fresh and stale */ ],
  reason: "<string>" /* present for stale and failed */,
  fetched_at: "<ISO-8601 string>" /* present for fresh and stale */
});
```

Notes:

- The webview's `settings.js` defines `window.SCRIBE_ON_RELEASE_LIST` early (before any `request_releases` is posted) so the host can always call it safely.
- Calling the function more than once is a valid scenario (Retry → success → Retry again); the function replaces the panel state each time.

### 2.3 Bootstrap injection (already covered by R8)

In addition to the call-and-response path above, on webview startup the host injects (via the pre-page-load script hook):

```js
window.SCRIBE_BOOTSTRAP = {
  version: "<env!(\"CARGO_PKG_VERSION\")>",
  platform: "<linux|macos|other>"
};
```

`settings.js` reads `window.SCRIBE_BOOTSTRAP.version` once on `DOMContentLoaded` and writes `Scribe v${version}` into `#sidebar-footer`.

## 3. Test obligations

The contracts above generate the following test obligations, all of which land in `tasks.md`:

1. **serde round-trip tests** for `Release`, `ReleaseListResultState::{Fresh, Stale, Failed}`, `ClientMessage::ListReleases`, `ServerMessage::ReleaseList` through msgpack-named, asserting field names match the wire examples in §1.2.
2. **transport-failure mapping test** for `request_release_list`: a deliberately closed Unix-socket peer must produce `ReleaseListResultState::Failed { reason }` rather than panicking.
3. **scheme-validation test** for the host's `open_external_url` handler: `javascript:`, `file:`, and similar schemes are dropped silently (or logged) and never reach the platform opener.
4. **bootstrap snapshot test**: with a known `CARGO_PKG_VERSION`, the injected bootstrap script equals the expected literal so the version-injection path cannot regress invisibly.

Each obligation is the basis for a corresponding entry in the next phase's `tasks.md`.

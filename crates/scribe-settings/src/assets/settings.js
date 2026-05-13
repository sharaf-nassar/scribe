// Scribe Settings UI — JavaScript

// ─────────── IPC ───────────

function sendIpc(message) {
  if (window.ipc && window.ipc.postMessage) {
    window.ipc.postMessage(JSON.stringify(message));
  }
}

function sendChange(key, value) {
  sendIpc({ type: "setting_changed", key, value });
}

function sendHostAction(type, extra) {
  var payload = Object.assign({ type: type }, extra || {});
  sendIpc(payload);
}

// ─────────── State ───────────

let currentConfig = {};
let keybindingDefaults = {};
let recordingEl = null;
let recordingPrev = null;
let recordingPrevText = null;
var themeColors = {};
var activeThemeId = null;
var isCustomMode = false;
window.SCRIBE_PLATFORM = window.SCRIBE_PLATFORM || "linux";

function rerenderAllKeybindingBadges() {
  var cells = document.querySelectorAll(".keybinding-cell[data-action]");
  cells.forEach(function(cell) {
    var action = cell.getAttribute("data-action");
    if (!action) { return; }
    renderBadges(cell, action, getCombosForAction(action));
  });
}

function setPlatform(platform) {
  window.SCRIBE_PLATFORM = platform || "linux";
  rerenderAllKeybindingBadges();
  updateNotificationPlatformRows();
}

function setPlatformVisibility(selector, shouldShow) {
  document.querySelectorAll(selector).forEach(function(el) {
    el.classList.toggle("platform-hidden", !shouldShow);
  });
}

function notificationTimeoutMode() {
  return currentConfig.notifications?.timeout_mode || "system_default";
}

function updateNotificationTimeoutVisibility() {
  var isLinux = window.SCRIBE_PLATFORM === "linux";
  var customTimeout = notificationTimeoutMode() === "custom";
  setPlatformVisibility("#notifications-timeout-secs-row", isLinux && customTimeout);
}

function updateNotificationPlatformRows() {
  var isLinux = window.SCRIBE_PLATFORM === "linux";
  var isMac = window.SCRIBE_PLATFORM === "macos";

  setPlatformVisibility(".platform-linux-only", isLinux);
  setPlatformVisibility(".platform-macos-only", isMac);
  updateNotificationTimeoutVisibility();
}

function syncNotificationConfigValue(key, value) {
  if (!currentConfig.notifications) {
    currentConfig.notifications = {};
  }

  var notificationsKey = key.slice("notifications.".length);
  currentConfig.notifications[notificationsKey] = value;
}

// ─────────── Smart Selection ───────────

var smartSelectionState = null;
var smartSelectionSelectedIndex = 0;

var SMART_PRECISIONS = [
  ["very_low", "Very Low"],
  ["low", "Low"],
  ["normal", "Normal"],
  ["high", "High"],
  ["very_high", "Very High"]
];

var SMART_ACTION_KINDS = [
  ["open_file", "Open File"],
  ["open_url", "Open URL"],
  ["run_command", "Run Command"],
  ["run_coprocess", "Run Coprocess"],
  ["send_text", "Send Text"],
  ["run_command_in_window", "Run Command in Window"],
  ["copy", "Copy"]
];

function cloneSmartSelection(value) {
  return JSON.parse(JSON.stringify(value));
}

function defaultSmartAction(kind, parameter) {
  return {
    kind: kind || "copy",
    parameter: parameter || "",
    parameter_mode: "legacy"
  };
}

function defaultSmartSelectionConfig() {
  return {
    activation: "quad_click",
    rules: [
      {
        id: "whitespace_word",
        name: "Whitespace-bounded word",
        enabled: true,
        regex: "\\S+",
        precision: "very_low",
        actions: []
      },
      {
        id: "namespace_identifier",
        name: "Namespace identifier",
        enabled: true,
        regex: "[A-Za-z_][A-Za-z0-9_]*(?:::[A-Za-z_][A-Za-z0-9_]*)+",
        precision: "normal",
        actions: []
      },
      {
        id: "path",
        name: "Path",
        enabled: true,
        regex: "(?:~|\\.{1,2})?/[^\\s\"'<>|]+|[A-Za-z0-9_.-]+(?:/[A-Za-z0-9_.-]+)+",
        precision: "high",
        actions: [defaultSmartAction("open_file")]
      },
      {
        id: "quoted_string",
        name: "Quoted string",
        enabled: true,
        regex: "\"(?:[^\"\\\\]|\\\\.)*\"|'(?:[^'\\\\]|\\\\.)*'",
        precision: "normal",
        actions: []
      },
      {
        id: "include_path",
        name: "Java/Python include path",
        enabled: true,
        regex: "[A-Za-z_][A-Za-z0-9_]*(?:\\.[A-Za-z_][A-Za-z0-9_]*){2,}",
        precision: "high",
        actions: []
      },
      {
        id: "uri",
        name: "URI",
        enabled: true,
        regex: "(?:mailto|https?|ssh|telnet):[^\\s\"'<>|]+",
        precision: "very_high",
        actions: [defaultSmartAction("open_url")]
      },
      {
        id: "objective_c_selector",
        name: "Objective-C selector",
        enabled: true,
        regex: "@selector\\([A-Za-z_][A-Za-z0-9_]*(?::[A-Za-z_][A-Za-z0-9_]*)*:?\\)|[A-Za-z_][A-Za-z0-9_]*(?::[A-Za-z_][A-Za-z0-9_]*)+:",
        precision: "high",
        actions: []
      },
      {
        id: "email",
        name: "Email address",
        enabled: true,
        regex: "[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\\.[A-Za-z]{2,}",
        precision: "very_high",
        actions: [defaultSmartAction("open_url", "mailto:\\0")]
      }
    ]
  };
}

function normalizeSmartSelectionConfig(config) {
  var fallback = defaultSmartSelectionConfig();
  var input = config && typeof config === "object" ? cloneSmartSelection(config) : fallback;
  if (input.activation !== "double_click" && input.activation !== "quad_click") {
    input.activation = "quad_click";
  }
  if (!Array.isArray(input.rules)) {
    input.rules = fallback.rules;
  }
  input.rules = input.rules.map(function(rule, index) {
    var normalized = rule && typeof rule === "object" ? rule : {};
    var precision = SMART_PRECISIONS.some(function(item) { return item[0] === normalized.precision; })
      ? normalized.precision
      : "normal";
    return {
      id: normalized.id || ("rule_" + String(index + 1)),
      name: normalized.name || "Untitled Rule",
      enabled: normalized.enabled !== false,
      regex: normalized.regex || "",
      precision: precision,
      actions: Array.isArray(normalized.actions) ? normalized.actions.map(normalizeSmartAction) : []
    };
  });
  return input;
}

function normalizeSmartAction(action) {
  var value = action && typeof action === "object" ? action : {};
  var kind = SMART_ACTION_KINDS.some(function(item) { return item[0] === value.kind; })
    ? value.kind
    : "copy";
  return {
    kind: kind,
    parameter: value.parameter || "",
    parameter_mode: value.parameter_mode === "interpolated" ? "interpolated" : "legacy"
  };
}

function selectedSmartRule() {
  if (!smartSelectionState || smartSelectionState.rules.length === 0) { return null; }
  smartSelectionSelectedIndex = Math.max(
    0,
    Math.min(smartSelectionSelectedIndex, smartSelectionState.rules.length - 1)
  );
  return smartSelectionState.rules[smartSelectionSelectedIndex];
}

function validateSmartRule(rule) {
  if (!rule || !rule.enabled) { return ""; }
  if (!rule.regex || rule.regex.trim().length === 0) {
    return "Regex is required for enabled rules.";
  }
  try {
    new RegExp(rule.regex);
  } catch (error) {
    return error.message;
  }
  return "";
}

function allSmartRulesValid() {
  if (!smartSelectionState) { return false; }
  for (var i = 0; i < smartSelectionState.rules.length; i++) {
    if (validateSmartRule(smartSelectionState.rules[i])) { return false; }
  }
  return true;
}

function sendSmartSelectionConfig() {
  if (!smartSelectionState || !allSmartRulesValid()) { return; }
  if (!currentConfig.terminal) { currentConfig.terminal = {}; }
  currentConfig.terminal.smart_selection = cloneSmartSelection(smartSelectionState);
  sendChange("terminal.smart_selection", smartSelectionState);
}

function renderSmartSelection() {
  if (!smartSelectionState) {
    smartSelectionState = defaultSmartSelectionConfig();
  }
  renderSmartActivation();
  renderSmartRuleList();
  renderSmartRuleEditor();
}

function renderSmartActivation() {
  var ctrl = document.getElementById("smart-selection-activation");
  if (!ctrl) { return; }
  ctrl.querySelectorAll(".smart-segment-opt").forEach(function(opt) {
    opt.classList.toggle("active", opt.getAttribute("data-value") === smartSelectionState.activation);
  });
}

function renderSmartRuleList() {
  var list = document.getElementById("smart-rule-list");
  if (!list || !smartSelectionState) { return; }
  while (list.firstChild) { list.removeChild(list.firstChild); }
  smartSelectionState.rules.forEach(function(rule, index) {
    var item = document.createElement("button");
    item.type = "button";
    item.className = "smart-rule-item" + (index === smartSelectionSelectedIndex ? " active" : "") + (rule.enabled ? "" : " disabled");
    item.setAttribute("data-index", String(index));
    var dot = document.createElement("span");
    dot.className = "smart-rule-enabled";
    var name = document.createElement("span");
    name.className = "smart-rule-name";
    name.textContent = rule.name || "Untitled Rule";
    item.appendChild(dot);
    item.appendChild(name);
    list.appendChild(item);
  });
}

function renderSmartRuleEditor() {
  var editor = document.getElementById("smart-rule-editor");
  var rule = selectedSmartRule();
  if (!editor) { return; }
  editor.style.display = rule ? "" : "none";
  if (!rule) { return; }

  document.getElementById("smart-rule-enabled").checked = rule.enabled !== false;
  document.getElementById("smart-rule-name").value = rule.name || "";
  document.getElementById("smart-rule-regex").value = rule.regex || "";
  document.getElementById("smart-rule-precision").value = rule.precision || "normal";
  renderSmartActionList(rule);
  renderSmartPreview();
}

function fillSelectOptions(select, items) {
  while (select.firstChild) { select.removeChild(select.firstChild); }
  items.forEach(function(item) {
    var option = document.createElement("option");
    option.value = item[0];
    option.textContent = item[1];
    select.appendChild(option);
  });
}

function renderSmartActionList(rule) {
  var list = document.getElementById("smart-action-list");
  if (!list) { return; }
  while (list.firstChild) { list.removeChild(list.firstChild); }
  rule.actions.forEach(function(action, index) {
    var row = document.createElement("div");
    row.className = "smart-action-row";
    row.setAttribute("data-index", String(index));

    var kind = document.createElement("select");
    kind.className = "select-control smart-select";
    kind.setAttribute("data-field", "kind");
    fillSelectOptions(kind, SMART_ACTION_KINDS);
    kind.value = action.kind || "copy";

    var parameter = document.createElement("input");
    parameter.type = "text";
    parameter.className = "text-input smart-input";
    parameter.setAttribute("data-field", "parameter");
    parameter.value = action.parameter || "";
    parameter.placeholder = "\\0";

    var mode = document.createElement("select");
    mode.className = "select-control smart-select";
    mode.setAttribute("data-field", "parameter_mode");
    fillSelectOptions(mode, [["legacy", "Legacy"], ["interpolated", "Interpolated"]]);
    mode.value = action.parameter_mode || "legacy";

    var buttons = document.createElement("div");
    buttons.className = "smart-action-buttons";
    [["up", "\u2191"], ["down", "\u2193"], ["duplicate", "Duplicate"], ["remove", "Remove"]]
      .forEach(function(item) {
        var btn = document.createElement("button");
        btn.type = "button";
        btn.className = "smart-tool-btn";
        btn.setAttribute("data-smart-action", item[0]);
        btn.textContent = item[1];
        buttons.appendChild(btn);
      });

    row.appendChild(kind);
    row.appendChild(parameter);
    row.appendChild(mode);
    row.appendChild(buttons);
    list.appendChild(row);
  });
}

function renderSmartPreview() {
  var rule = selectedSmartRule();
  var validation = document.getElementById("smart-rule-validation");
  var preview = document.getElementById("smart-preview-result");
  var sample = document.getElementById("smart-sample-text");
  if (!rule || !validation || !preview || !sample) { return; }

  var error = validateSmartRule(rule);
  validation.textContent = error;
  if (error) {
    preview.textContent = "No match";
    return;
  }

  var regex = new RegExp(rule.regex);
  var match = regex.exec(sample.value || "");
  preview.textContent = match ? ("Match: " + match[0]) : "No match";
}

function updateSelectedSmartRule(mutator) {
  var rule = selectedSmartRule();
  if (!rule) { return; }
  mutator(rule);
  renderSmartSelection();
  sendSmartSelectionConfig();
}

function makeSmartRuleId() {
  return "custom_" + Date.now().toString(36) + "_" + Math.floor(Math.random() * 10000).toString(36);
}

function initSmartSelection() {
  var activation = document.getElementById("smart-selection-activation");
  if (activation) {
    activation.addEventListener("click", function(event) {
      var opt = event.target.closest(".smart-segment-opt");
      if (!opt) { return; }
      smartSelectionState.activation = opt.getAttribute("data-value") || "quad_click";
      renderSmartActivation();
      sendSmartSelectionConfig();
    });
  }

  var list = document.getElementById("smart-rule-list");
  if (list) {
    list.addEventListener("click", function(event) {
      var item = event.target.closest(".smart-rule-item");
      if (!item) { return; }
      smartSelectionSelectedIndex = parseInt(item.getAttribute("data-index") || "0", 10);
      renderSmartSelection();
    });
  }

  var enabled = document.getElementById("smart-rule-enabled");
  if (enabled) {
    enabled.addEventListener("change", function() {
      updateSelectedSmartRule(function(rule) { rule.enabled = enabled.checked; });
    });
  }

  var name = document.getElementById("smart-rule-name");
  if (name) {
    name.addEventListener("input", function() {
      var rule = selectedSmartRule();
      if (!rule) { return; }
      rule.name = name.value;
      renderSmartRuleList();
      sendSmartSelectionConfig();
    });
  }

  var regex = document.getElementById("smart-rule-regex");
  if (regex) {
    regex.addEventListener("input", function() {
      var rule = selectedSmartRule();
      if (!rule) { return; }
      rule.regex = regex.value;
      renderSmartPreview();
      renderSmartRuleList();
      sendSmartSelectionConfig();
    });
  }

  var precision = document.getElementById("smart-rule-precision");
  if (precision) {
    precision.addEventListener("change", function() {
      updateSelectedSmartRule(function(rule) { rule.precision = precision.value; });
    });
  }

  var sample = document.getElementById("smart-sample-text");
  if (sample) {
    sample.addEventListener("input", renderSmartPreview);
  }

  var add = document.getElementById("smart-rule-add");
  if (add) {
    add.addEventListener("click", function() {
      smartSelectionState.rules.push({
        id: makeSmartRuleId(),
        name: "New Rule",
        enabled: true,
        regex: "\\S+",
        precision: "normal",
        actions: [defaultSmartAction("copy")]
      });
      smartSelectionSelectedIndex = smartSelectionState.rules.length - 1;
      renderSmartSelection();
      sendSmartSelectionConfig();
    });
  }

  var duplicate = document.getElementById("smart-rule-duplicate");
  if (duplicate) {
    duplicate.addEventListener("click", function() {
      var rule = selectedSmartRule();
      if (!rule) { return; }
      var copy = cloneSmartSelection(rule);
      copy.id = makeSmartRuleId();
      copy.name = copy.name + " Copy";
      smartSelectionState.rules.splice(smartSelectionSelectedIndex + 1, 0, copy);
      smartSelectionSelectedIndex += 1;
      renderSmartSelection();
      sendSmartSelectionConfig();
    });
  }

  var remove = document.getElementById("smart-rule-remove");
  if (remove) {
    remove.addEventListener("click", function() {
      if (!smartSelectionState || smartSelectionState.rules.length === 0) { return; }
      smartSelectionState.rules.splice(smartSelectionSelectedIndex, 1);
      smartSelectionSelectedIndex = Math.max(0, smartSelectionSelectedIndex - 1);
      renderSmartSelection();
      sendSmartSelectionConfig();
    });
  }

  var up = document.getElementById("smart-rule-up");
  var down = document.getElementById("smart-rule-down");
  if (up) { up.addEventListener("click", function() { moveSmartRule(-1); }); }
  if (down) { down.addEventListener("click", function() { moveSmartRule(1); }); }

  var restore = document.getElementById("smart-rule-restore");
  if (restore) {
    restore.addEventListener("click", function() {
      smartSelectionState = defaultSmartSelectionConfig();
      smartSelectionSelectedIndex = 0;
      if (!currentConfig.terminal) { currentConfig.terminal = {}; }
      currentConfig.terminal.smart_selection = cloneSmartSelection(smartSelectionState);
      renderSmartSelection();
      sendChange("terminal.smart_selection.reset", true);
    });
  }

  var addAction = document.getElementById("smart-action-add");
  if (addAction) {
    addAction.addEventListener("click", function() {
      updateSelectedSmartRule(function(rule) {
        rule.actions.push(defaultSmartAction("copy"));
      });
    });
  }

  var actionList = document.getElementById("smart-action-list");
  if (actionList) {
    actionList.addEventListener("input", onSmartActionInput);
    actionList.addEventListener("change", onSmartActionInput);
    actionList.addEventListener("click", onSmartActionClick);
  }
}

function moveSmartRule(delta) {
  if (!smartSelectionState) { return; }
  var from = smartSelectionSelectedIndex;
  var to = from + delta;
  if (to < 0 || to >= smartSelectionState.rules.length) { return; }
  var moved = smartSelectionState.rules.splice(from, 1)[0];
  smartSelectionState.rules.splice(to, 0, moved);
  smartSelectionSelectedIndex = to;
  renderSmartSelection();
  sendSmartSelectionConfig();
}

function onSmartActionInput(event) {
  var row = event.target.closest(".smart-action-row");
  if (!row) { return; }
  var field = event.target.getAttribute("data-field");
  if (!field) { return; }
  var rule = selectedSmartRule();
  if (!rule) { return; }
  var index = parseInt(row.getAttribute("data-index") || "0", 10);
  var action = rule.actions[index];
  if (!action) { return; }
  action[field] = event.target.value;
  sendSmartSelectionConfig();
}

function onSmartActionClick(event) {
  var button = event.target.closest("[data-smart-action]");
  if (!button) { return; }
  var row = button.closest(".smart-action-row");
  var rule = selectedSmartRule();
  if (!row || !rule) { return; }
  var index = parseInt(row.getAttribute("data-index") || "0", 10);
  var command = button.getAttribute("data-smart-action");
  if (command === "remove") {
    rule.actions.splice(index, 1);
  } else if (command === "duplicate" && rule.actions[index]) {
    rule.actions.splice(index + 1, 0, cloneSmartSelection(rule.actions[index]));
  } else if (command === "up" && index > 0) {
    var upAction = rule.actions.splice(index, 1)[0];
    rule.actions.splice(index - 1, 0, upAction);
  } else if (command === "down" && index < rule.actions.length - 1) {
    var downAction = rule.actions.splice(index, 1)[0];
    rule.actions.splice(index + 1, 0, downAction);
  }
  renderSmartRuleEditor();
  sendSmartSelectionConfig();
}

// ─────────── Releases (US1 + US3) ───────────
//
// State is held at file scope so the tab-activation handler, the picker change
// handler, the Older/Newer buttons, and the host-injected `SCRIBE_ON_RELEASE_LIST`
// callback all share a single source of truth.
//
// `releases` is a newest-first array (the server returns it in that order).
// `selectedReleaseVersion` is the canonical pointer — it is set by the picker,
// the navigation buttons, and the auto-select-most-recent rule (FR-008).
// `releaseListState` is "idle" until the panel is first opened; transitions
// to "loading" while we wait on the host, and then "fresh" / "stale" / "failed"
// based on the SCRIBE_ON_RELEASE_LIST payload.
var releases = [];
var selectedReleaseVersion = null;
var releaseListState = "idle"; // "idle"|"loading"|"fresh"|"stale"|"failed"
var releaseLastFetchedAt = null;

// Defined early so the host's `evaluate_script` call can always reach it,
// even if it fires before the rest of the script runs to its end.
window.SCRIBE_ON_RELEASE_LIST = function(payload) {
  onReleaseList(payload || {});
};

// Render pre-sanitized HTML coming from the host.
// The server runs `body_html` through pulldown-cmark + ammonia
// (see crates/scribe-server/src/releases.rs), so the string is safe to
// mount as-is. All HTML insertion in this file goes through this single
// helper to keep the trust boundary explicit.
function setHostSanitizedHtml(el, html) {
  if (!el) { return; }
  el.innerHTML = html;
}

function onReleaseList(payload) {
  var state = payload.state;
  if (state === "fresh") {
    releases = Array.isArray(payload.releases) ? payload.releases : [];
    releaseLastFetchedAt = payload.fetched_at || null;
    releaseListState = "fresh";
    ensureSelectedReleaseValid();
    populatePicker(releases);
    renderRelease();
    updateNavBoundaries();
    hideReleasesStatus();
  } else if (state === "stale") {
    releases = Array.isArray(payload.releases) ? payload.releases : [];
    releaseLastFetchedAt = payload.fetched_at || null;
    releaseListState = "stale";
    ensureSelectedReleaseValid();
    populatePicker(releases);
    renderRelease();
    updateNavBoundaries();
    showStaleStatus(payload.reason || "unknown reason", releaseLastFetchedAt);
  } else if (state === "failed") {
    releaseListState = "failed";
    if (releases.length === 0) {
      setHostSanitizedHtml(document.getElementById("release-notes"), "");
    }
    showFailedStatus(payload.reason || "unknown error");
  } else {
    // Unknown payload — render as a generic failure.
    releaseListState = "failed";
    showFailedStatus("unexpected response from host");
  }
}

function ensureSelectedReleaseValid() {
  if (releases.length === 0) {
    selectedReleaseVersion = null;
    return;
  }
  var stillExists = selectedReleaseVersion !== null
    && releases.some(function(r) { return r.version === selectedReleaseVersion; });
  if (!stillExists) {
    selectedReleaseVersion = releases[0].version;
  }
}

function formatReleaseDate(iso) {
  if (typeof iso !== "string" || iso.length === 0) { return ""; }
  var d = new Date(iso);
  if (isNaN(d.getTime())) { return ""; }
  var y = d.getUTCFullYear();
  var m = String(d.getUTCMonth() + 1).padStart(2, "0");
  var day = String(d.getUTCDate()).padStart(2, "0");
  return y + "-" + m + "-" + day;
}

function populatePicker(list) {
  var picker = document.getElementById("releases-picker");
  if (!picker) { return; }
  while (picker.firstChild) {
    picker.removeChild(picker.firstChild);
  }
  for (var i = 0; i < list.length; i++) {
    var r = list[i];
    var opt = document.createElement("option");
    opt.value = r.version;
    var date = formatReleaseDate(r.published_at);
    var label = "v" + r.version;
    if (date) { label += " — " + date; }
    if (r.prerelease) { label = "[PRE] " + label; }
    opt.textContent = label;
    picker.appendChild(opt);
  }
}

function renderRelease() {
  var notesEl = document.getElementById("release-notes");
  var picker = document.getElementById("releases-picker");
  var external = document.getElementById("releases-external");
  if (!notesEl || !picker || !external) { return; }
  if (selectedReleaseVersion === null) { return; }

  var rel = null;
  for (var i = 0; i < releases.length; i++) {
    if (releases[i].version === selectedReleaseVersion) {
      rel = releases[i];
      break;
    }
  }
  if (!rel) { return; }

  // body_html is already sanitized server-side via ammonia.
  // Re-sanitization in JS is explicitly disallowed by the feature spec.
  var bodyHtml = typeof rel.body_html === "string" ? rel.body_html : "";
  if (bodyHtml.length === 0) {
    setHostSanitizedHtml(notesEl, '<p class="release-notes-empty">No release notes provided for this version.</p>');
  } else {
    setHostSanitizedHtml(notesEl, bodyHtml);
  }

  if (picker.value !== rel.version) {
    picker.value = rel.version;
  }
  if (typeof rel.html_url === "string" && rel.html_url.length > 0) {
    external.setAttribute("href", rel.html_url);
  } else {
    external.setAttribute("href", "#");
  }
}

function updateNavBoundaries() {
  var newer = document.getElementById("releases-newer");
  var older = document.getElementById("releases-older");
  if (!newer || !older) { return; }
  var idx = -1;
  for (var i = 0; i < releases.length; i++) {
    if (releases[i].version === selectedReleaseVersion) { idx = i; break; }
  }
  // Newer = lower index (the list is newest-first). Disable when at index 0
  // or when nothing is selected.
  newer.disabled = (idx <= 0);
  // Older = higher index. Disable at the tail or when nothing is selected.
  older.disabled = (idx < 0 || idx >= releases.length - 1);
}

function hideReleasesStatus() {
  var el = document.getElementById("releases-status");
  if (!el) { return; }
  el.hidden = true;
  el.className = "releases-status";
  while (el.firstChild) { el.removeChild(el.firstChild); }
}

function setReleasesStatus(variant, message, buttonLabel, onButtonClick) {
  var el = document.getElementById("releases-status");
  if (!el) { return; }
  el.hidden = false;
  el.className = "releases-status " + variant;
  while (el.firstChild) { el.removeChild(el.firstChild); }
  var msg = document.createElement("span");
  msg.className = "releases-status-message";
  msg.textContent = message;
  el.appendChild(msg);
  if (buttonLabel && typeof onButtonClick === "function") {
    var btn = document.createElement("button");
    btn.type = "button";
    btn.className = "releases-nav-btn";
    btn.textContent = buttonLabel;
    btn.addEventListener("click", onButtonClick);
    el.appendChild(btn);
  }
}

function showLoadingStatus() {
  setReleasesStatus("is-loading", "Loading releases…", null, null);
}

function showStaleStatus(reason, fetchedAt) {
  var label = "Showing cached releases";
  if (fetchedAt) { label += " (last refreshed at " + fetchedAt + ")"; }
  if (reason) { label += " — " + reason; }
  label += ".";
  setReleasesStatus("is-stale", label, "Refresh", function() {
    requestReleases();
  });
}

function showFailedStatus(reason) {
  setReleasesStatus("is-error", "Could not load releases: " + reason, "Retry", function() {
    requestReleases();
  });
}

function requestReleases() {
  releaseListState = "loading";
  showLoadingStatus();
  sendIpc({ type: "request_releases" });
}

function initReleasesPanel() {
  var picker = document.getElementById("releases-picker");
  var newer = document.getElementById("releases-newer");
  var older = document.getElementById("releases-older");
  var page = document.getElementById("page-releases");

  if (picker) {
    picker.addEventListener("change", function(e) {
      selectedReleaseVersion = e.target.value;
      renderRelease();
      updateNavBoundaries();
    });
  }

  if (newer) {
    newer.addEventListener("click", function() {
      // step toward newer = lower index in our newest-first list
      var i = -1;
      for (var k = 0; k < releases.length; k++) {
        if (releases[k].version === selectedReleaseVersion) { i = k; break; }
      }
      if (i > 0) {
        selectedReleaseVersion = releases[i - 1].version;
        renderRelease();
        updateNavBoundaries();
      }
    });
  }

  if (older) {
    older.addEventListener("click", function() {
      var i = -1;
      for (var k = 0; k < releases.length; k++) {
        if (releases[k].version === selectedReleaseVersion) { i = k; break; }
      }
      if (i >= 0 && i < releases.length - 1) {
        selectedReleaseVersion = releases[i + 1].version;
        renderRelease();
        updateNavBoundaries();
      }
    });
  }

  // Delegate any <a href> click inside the releases page (panel + title row)
  // to the host so the OS browser opens it instead of the webview. Covers
  // both the rendered notes' inline links and the [data-external] "View on
  // GitHub" header link, which sits in the page-header row outside the
  // panel itself.
  if (page) {
    page.addEventListener("click", function(e) {
      var anchor = e.target && e.target.closest ? e.target.closest("a") : null;
      if (!anchor) { return; }
      if (!page.contains(anchor)) { return; }
      var href = anchor.getAttribute("href");
      if (!href || href === "#") { return; }
      e.preventDefault();
      sendIpc({ type: "open_external_url", url: anchor.href });
    });
  }
}

// ─────────── Theme Grid ───────────

function makeColorSpan(text, color) {
  var span = document.createElement("span");
  span.textContent = text;
  span.style.color = color;
  return span;
}

function makePreviewLine(parts) {
  var div = document.createElement("div");
  parts.forEach(function(p, i) {
    if (i > 0) { div.appendChild(document.createTextNode(" ")); }
    div.appendChild(makeColorSpan(p.text, p.color));
  });
  return div;
}

function loadThemeColors(data) {
  themeColors = data;
  if (!isCustomMode && themeColors[activeThemeId]) {
    populateColorEditor(themeColors[activeThemeId]);
  }
  if (currentConfig && Object.keys(currentConfig).length > 0) {
    syncPromptBarSwatches(currentConfig);
  }
  renderThemeGrid();
}

function renderThemeGrid() {
  var grid = document.getElementById("theme-grid");
  if (!grid) { return; }

  var searchEl = document.getElementById("theme-search");
  var filter = searchEl ? searchEl.value.toLowerCase() : "";

  while (grid.firstChild) { grid.removeChild(grid.firstChild); }

  var ids = Object.keys(themeColors);
  ids.sort(function(a, b) {
    var na = themeColors[a].name.toLowerCase();
    var nb = themeColors[b].name.toLowerCase();
    if (na < nb) { return -1; }
    if (na > nb) { return 1; }
    return 0;
  });

  // Move active theme to front (unless custom mode, which prepends its own card)
  if (!isCustomMode && activeThemeId && ids.indexOf(activeThemeId) !== -1) {
    ids.splice(ids.indexOf(activeThemeId), 1);
    ids.unshift(activeThemeId);
  }

  // Prepend Custom card when in custom mode
  if (isCustomMode) {
    var customCard = buildCustomCard();
    grid.appendChild(customCard);
  }

  ids.forEach(function(id) {
    var spec = themeColors[id];
    if (!spec) { return; }
    if (filter && spec.name.toLowerCase().indexOf(filter) === -1) { return; }

    var card = document.createElement("div");
    card.className = "theme-card";
    if (!isCustomMode && id === activeThemeId) { card.className += " selected"; }
    card.setAttribute("data-theme", id);

    var preview = document.createElement("div");
    preview.className = "theme-preview";
    preview.style.background = spec.bg;

    var ansi = spec.ansi || [];
    var green = ansi[2] || spec.fg;
    var blue = ansi[4] || spec.fg;
    var red = ansi[1] || spec.fg;
    var yellow = ansi[3] || spec.fg;
    var muted = ansi[8] || "#71717a";

    preview.appendChild(makePreviewLine([
      {text: "$", color: green}, {text: "cargo build", color: spec.fg}
    ]));
    preview.appendChild(makePreviewLine([
      {text: "src/", color: blue}, {text: "Cargo.toml", color: yellow}
    ]));
    preview.appendChild(makePreviewLine([
      {text: "error", color: red}, {text: "fn", color: blue}, {text: "main()", color: muted}
    ]));

    var nameEl = document.createElement("div");
    nameEl.className = "theme-name";
    var nameSpan = document.createElement("span");
    nameSpan.textContent = spec.name;
    var checkDiv = document.createElement("div");
    checkDiv.className = "theme-check";
    checkDiv.style.display = (!isCustomMode && id === activeThemeId) ? "flex" : "none";
    checkDiv.textContent = "\u2713";
    nameEl.appendChild(nameSpan);
    nameEl.appendChild(checkDiv);

    card.appendChild(preview);
    card.appendChild(nameEl);
    grid.appendChild(card);

    card.addEventListener("click", function() {
      selectTheme(id);
    });
  });
}

function buildCustomCard() {
  var bg = getSwatchColor("theme.background") || "#0e0e10";
  var fg = getSwatchColor("theme.foreground") || "#e4e4e7";
  var green = getAnsiSwatchColor("theme.ansi_normal.2") || "#22c55e";
  var blue = getAnsiSwatchColor("theme.ansi_normal.4") || "#3b82f6";
  var red = getAnsiSwatchColor("theme.ansi_normal.1") || "#ef4444";
  var yellow = getAnsiSwatchColor("theme.ansi_normal.3") || "#eab308";
  var muted = getAnsiSwatchColor("theme.ansi_bright.0") || "#52525b";

  var card = document.createElement("div");
  card.className = "theme-card selected";
  card.setAttribute("data-theme", "custom");

  var preview = document.createElement("div");
  preview.className = "theme-preview";
  preview.style.background = bg;

  preview.appendChild(makePreviewLine([
    {text: "$", color: green}, {text: "cargo build", color: fg}
  ]));
  preview.appendChild(makePreviewLine([
    {text: "src/", color: blue}, {text: "Cargo.toml", color: yellow}
  ]));
  preview.appendChild(makePreviewLine([
    {text: "error", color: red}, {text: "fn", color: blue}, {text: "main()", color: muted}
  ]));

  var nameEl = document.createElement("div");
  nameEl.className = "theme-name";
  var nameSpan = document.createElement("span");
  nameSpan.textContent = "Custom";
  var checkDiv = document.createElement("div");
  checkDiv.className = "theme-check";
  checkDiv.style.display = "flex";
  checkDiv.textContent = "\u2713";
  nameEl.appendChild(nameSpan);
  nameEl.appendChild(checkDiv);

  card.appendChild(preview);
  card.appendChild(nameEl);
  return card;
}

function getSwatchColor(key) {
  var el = document.querySelector(".color-swatch[data-key='" + key + "'] input[type='color']");
  return el ? el.value : null;
}

function getAnsiSwatchColor(key) {
  var el = document.querySelector(".ansi-swatch[data-key='" + key + "'] input[type='color']");
  return el ? el.value : null;
}

function selectTheme(id) {
  activeThemeId = id;
  isCustomMode = false;
  var spec = themeColors[id];
  if (spec) { populateColorEditor(spec); }
  sendChange("theme.preset", id);
  renderThemeGrid();
  syncPromptBarSwatches(currentConfig);
}

function populateColorEditor(spec) {
  if (!spec) { return; }
  setColorSwatch("theme.foreground", spec.fg);
  setColorSwatch("theme.background", spec.bg);
  setColorSwatch("theme.cursor", spec.cursor);
  setColorSwatch("theme.cursor_text", spec.cursor_accent);
  setColorSwatch("theme.selection", spec.selection);
  setColorSwatch("theme.selection_text", spec.selection_fg);
  if (spec.ansi) {
    for (var i = 0; i < 8; i++) {
      setAnsiSwatch("theme.ansi_normal." + i, spec.ansi[i]);
    }
    for (var j = 0; j < 8; j++) {
      setAnsiSwatch("theme.ansi_bright." + j, spec.ansi[j + 8]);
    }
  }
}

function clampChannel(value) {
  return Math.max(0, Math.min(255, Math.round(value)));
}

function hexToRgb(hex) {
  if (!hex || typeof hex !== "string") { return null; }
  var normalized = hex.charAt(0) === "#" ? hex.slice(1) : hex;
  if (normalized.length !== 6) { return null; }
  return {
    r: parseInt(normalized.slice(0, 2), 16),
    g: parseInt(normalized.slice(2, 4), 16),
    b: parseInt(normalized.slice(4, 6), 16)
  };
}

function rgbToHex(rgb) {
  if (!rgb) { return "#000000"; }
  return "#" + [rgb.r, rgb.g, rgb.b].map(function(channel) {
    return clampChannel(channel).toString(16).padStart(2, "0");
  }).join("");
}

function lightenHex(hex, amount) {
  var rgb = hexToRgb(hex);
  if (!rgb) { return "#151528"; }
  return rgbToHex({
    r: rgb.r + amount * 255,
    g: rgb.g + amount * 255,
    b: rgb.b + amount * 255
  });
}

function darkenHex(hex, amount) {
  var rgb = hexToRgb(hex);
  if (!rgb) { return "#0f0f20"; }
  return rgbToHex({
    r: rgb.r - amount * 255,
    g: rgb.g - amount * 255,
    b: rgb.b - amount * 255
  });
}

function activeThemeSpec() {
  if (isCustomMode) {
    var savedAnsi = Array.isArray(currentConfig.theme?.colors) ? currentConfig.theme.colors : [];
    return {
      fg: getSwatchColor("theme.foreground") || currentConfig.theme?.foreground || "#e4e4e7",
      bg: getSwatchColor("theme.background") || currentConfig.theme?.background || "#0e0e10",
      ansi: [
        getAnsiSwatchColor("theme.ansi_normal.0") || savedAnsi[0] || "#27272a",
        getAnsiSwatchColor("theme.ansi_normal.1") || savedAnsi[1] || "#ef4444",
        getAnsiSwatchColor("theme.ansi_normal.2") || savedAnsi[2] || "#22c55e",
        getAnsiSwatchColor("theme.ansi_normal.3") || savedAnsi[3] || "#eab308",
        getAnsiSwatchColor("theme.ansi_normal.4") || savedAnsi[4] || "#3b82f6"
      ]
    };
  }
  if (themeColors[activeThemeId]) {
    return themeColors[activeThemeId];
  }
  return {
    fg: "#e4e4e7",
    bg: "#0e0e10",
    ansi: ["#27272a", "#ef4444", "#22c55e", "#eab308", "#3b82f6"]
  };
}

function alphaColorString(hex, alpha) {
  var rgb = hexToRgb(hex);
  if (!rgb) { return "rgba(142, 142, 181, 0.5)"; }
  return "rgba(" + clampChannel(rgb.r) + ", " + clampChannel(rgb.g) + ", " + clampChannel(rgb.b) + ", " + alpha + ")";
}

function getPromptBarThemeDefaults() {
  var spec = activeThemeSpec();
  var background = spec.bg || "#0e0e10";
  var foreground = spec.fg || "#e4e4e7";
  var ansi = Array.isArray(spec.ansi) ? spec.ansi : [];
  return {
    firstRow: darkenHex(background, 0.02),
    secondRow: lightenHex(background, 0.03),
    textSwatch: alphaColorString(foreground, 0.5),
    textInput: foreground,
    iconFirst: ansi[3] || foreground,
    iconLatest: ansi[4] || foreground
  };
}

function syncPromptBarSwatches(config) {
  var defaults = getPromptBarThemeDefaults();
  var secondRowOverride =
    config.appearance?.prompt_bar_second_row_bg || config.appearance?.prompt_bar_bg;
  var firstRowOverride = config.appearance?.prompt_bar_first_row_bg;
  var textOverride = config.appearance?.prompt_bar_text;
  var iconFirstOverride = config.appearance?.prompt_bar_icon_first;
  var iconLatestOverride = config.appearance?.prompt_bar_icon_latest;

  if (secondRowOverride) {
    setColorSwatch("appearance.prompt_bar_second_row_bg", secondRowOverride);
  } else {
    setColorSwatchVisual(
      "appearance.prompt_bar_second_row_bg",
      defaults.secondRow,
      defaults.secondRow
    );
  }

  if (firstRowOverride) {
    setColorSwatch("appearance.prompt_bar_first_row_bg", firstRowOverride);
  } else {
    setColorSwatchVisual(
      "appearance.prompt_bar_first_row_bg",
      defaults.firstRow,
      defaults.firstRow
    );
  }

  if (textOverride) {
    setColorSwatch("appearance.prompt_bar_text", textOverride);
  } else {
    setColorSwatchVisual("appearance.prompt_bar_text", defaults.textSwatch, defaults.textInput);
  }

  if (iconFirstOverride) {
    setColorSwatch("appearance.prompt_bar_icon_first", iconFirstOverride);
  } else {
    setColorSwatchVisual(
      "appearance.prompt_bar_icon_first",
      defaults.iconFirst,
      defaults.iconFirst
    );
  }

  if (iconLatestOverride) {
    setColorSwatch("appearance.prompt_bar_icon_latest", iconLatestOverride);
  } else {
    setColorSwatchVisual(
      "appearance.prompt_bar_icon_latest",
      defaults.iconLatest,
      defaults.iconLatest
    );
  }
}

function enterCustomMode() {
  if (isCustomMode) { return; }
  isCustomMode = true;
  activeThemeId = "custom";
  sendChange("theme.preset", "custom");
  renderThemeGrid();
}

function initThemeColorEditor() {
  var editor = document.getElementById("theme-color-editor");
  if (!editor) { return; }

  editor.querySelectorAll(".color-swatch").forEach(function(swatch) {
    var colorInput = swatch.querySelector("input[type='color']");
    if (!colorInput) { return; }
    colorInput.addEventListener("change", function() {
      enterCustomMode();
      syncPromptBarSwatches(currentConfig);
    });
  });

  editor.querySelectorAll(".ansi-swatch").forEach(function(swatch) {
    var colorInput = swatch.querySelector("input[type='color']");
    if (!colorInput) { return; }
    colorInput.addEventListener("change", function() {
      enterCustomMode();
      syncPromptBarSwatches(currentConfig);
    });
  });

  var searchEl = document.getElementById("theme-search");
  if (searchEl) {
    searchEl.addEventListener("input", function() {
      renderThemeGrid();
    });
  }
}

// ─────────── Font List (injected by Rust) ───────────

function loadFontList(fonts) {
  const select = document.querySelector('select[data-key="appearance.font_family"]');
  if (!select) return;

  const currentValue = select.value;

  // Clear existing options safely (no innerHTML).
  while (select.firstChild) {
    select.removeChild(select.firstChild);
  }

  for (const name of fonts) {
    const opt = document.createElement("option");
    opt.value = name;
    opt.textContent = name;
    select.appendChild(opt);
  }

  // Always include a System Monospace fallback.
  const fallback = document.createElement("option");
  fallback.value = "monospace";
  fallback.textContent = "System Monospace";
  select.appendChild(fallback);

  // Restore the previously selected value if it exists in the new list.
  const configValue = currentConfig.appearance?.font || currentValue;
  if (configValue) {
    const match = Array.from(select.options).find(o => o.value === configValue);
    if (match) {
      select.value = configValue;
    } else {
      // Font not installed — add it as a visible entry so the user knows.
      const missing = document.createElement("option");
      missing.value = configValue;
      missing.textContent = configValue + " (not found)";
      select.insertBefore(missing, select.firstChild);
      select.value = configValue;
    }
  }
}

function requestFontRefresh() {
  sendHostAction("request_fonts");
}

function initNotificationActions() {
  var macSettingsBtn = document.getElementById("notifications-open-macos-settings");
  if (macSettingsBtn) {
    macSettingsBtn.addEventListener("click", function() {
      sendHostAction("open_macos_notification_settings");
    });
  }
}

// ─────────── Manual Update Check ───────────

var UPDATE_CHECK_DEFAULT_TEXT = "Click to query GitHub releases for the active channel";

function setUpdateCheckStatus(text, variant) {
  var el = document.getElementById("update-check-status");
  if (!el) { return; }
  el.textContent = "";
  el.classList.remove("is-checking", "is-success", "is-error");
  if (variant) { el.classList.add(variant); }
  if (typeof text === "string") {
    el.textContent = text;
  }
}

function setUpdateCheckButtonBusy(busy) {
  var btn = document.getElementById("update-check-now-btn");
  if (!btn) { return; }
  btn.disabled = !!busy;
  btn.textContent = busy ? "Checking…" : "Check Now";
}

function initUpdateActions() {
  var btn = document.getElementById("update-check-now-btn");
  if (!btn) { return; }
  btn.addEventListener("click", function() {
    setUpdateCheckButtonBusy(true);
    setUpdateCheckStatus("Checking GitHub for the latest release…", "is-checking");
    sendHostAction("request_update_check");
  });
}

// Called from Rust after a manual check completes. `result` is the JSON form
// of `UpdateCheckResultState` produced by serde with default external tagging:
//   "NoUpdate"                                                → no update
//   { "UpdateAvailable": { "version": "1.2.3", "release_url": "https://…" } }
//   { "Failed": { "reason": "…" } }
function updateCheckResult(result) {
  setUpdateCheckButtonBusy(false);

  if (result === "NoUpdate") {
    var channel = currentConfig.update?.channel || "stable";
    setUpdateCheckStatus("You're up to date on the " + channel + " channel", "is-success");
    return;
  }

  if (result && typeof result === "object" && result.UpdateAvailable) {
    renderUpdateAvailable(result.UpdateAvailable);
    return;
  }

  if (result && typeof result === "object" && result.Failed) {
    var reason = result.Failed.reason || "unknown error";
    setUpdateCheckStatus("Check failed: " + reason, "is-error");
    return;
  }

  setUpdateCheckStatus(UPDATE_CHECK_DEFAULT_TEXT, null);
}

function renderUpdateAvailable(payload) {
  setUpdateCheckStatus("Update available: ", "is-success");
  var statusEl = document.getElementById("update-check-status");
  if (!statusEl) { return; }
  var version = payload.version || "latest";
  var url = payload.release_url || "";
  if (url) {
    var link = document.createElement("a");
    link.href = url;
    link.target = "_blank";
    link.rel = "noopener noreferrer";
    link.className = "update-check-link";
    link.textContent = "v" + version;
    statusEl.appendChild(link);
  } else {
    statusEl.appendChild(document.createTextNode("v" + version));
  }
}

// ─────────── Keybinding Defaults (injected by Rust) ───────────

function loadKeybindingDefaults(defaults) {
  keybindingDefaults = defaults;
}

// ─────────── Tab Navigation ───────────

function initNavigation() {
  const navItems = document.querySelectorAll(".nav-item");
  const pages = document.querySelectorAll(".content-page");

  navItems.forEach(function(item) {
    item.addEventListener("click", function() {
      const target = item.getAttribute("data-tab");

      navItems.forEach(function(n) { n.classList.remove("active"); });
      item.classList.add("active");

      pages.forEach(function(p) {
        if (p.id === "page-" + target) {
          p.classList.add("active");
        } else {
          p.classList.remove("active");
        }
      });

      // First time the user opens Releases we kick off the IPC fetch.
      // Subsequent activations just re-show the cached JS state.
      if (target === "releases") {
        if (releases.length === 0 && releaseListState !== "loading") {
          requestReleases();
        }
      }
    });
  });

  var promptBarColorsCrosslink = document.getElementById("prompt-bar-colors-crosslink");
  if (promptBarColorsCrosslink) {
    promptBarColorsCrosslink.addEventListener("click", function() {
      var colorsNav = document.querySelector('.nav-item[data-tab="colors"]');
      if (colorsNav) {
        colorsNav.click();
        var target = document.getElementById("colors-prompt-bar");
        if (target) { target.scrollIntoView({ behavior: "smooth", block: "start" }); }
      }
    });
  }
}

// ─────────── Number Steppers ───────────

function initSteppers() {
  document.querySelectorAll(".number-control").forEach(function(ctrl) {
    const key = ctrl.getAttribute("data-key");
    const step = parseFloat(ctrl.getAttribute("data-step") || "1");
    const min = parseFloat(ctrl.getAttribute("data-min") || "0");
    const max = parseFloat(ctrl.getAttribute("data-max") || "99999");

    // Replace the static <div> with an <input> so users can type values.
    const oldDiv = ctrl.querySelector(".number-value");
    const input = document.createElement("input");
    input.type = "text";
    input.inputMode = "numeric";
    input.className = "number-value";
    input.value = oldDiv.textContent.trim();
    oldDiv.replaceWith(input);

    const valueEl = input;
    const btns = ctrl.querySelectorAll(".number-btn");

    function clamp(v) {
      var n = parseFloat(v);
      if (isNaN(n)) { n = min; }
      n = Math.max(min, Math.min(max, n));
      n = Math.round(n / step) * step;
      return n;
    }

    btns[0].addEventListener("click", function() {
      var val = clamp(parseFloat(valueEl.value) - step);
      valueEl.value = String(val);
      if (key.indexOf("notifications.") === 0) {
        syncNotificationConfigValue(key, val);
      }
      sendChange(key, val);
    });

    btns[1].addEventListener("click", function() {
      var val = clamp(parseFloat(valueEl.value) + step);
      valueEl.value = String(val);
      if (key.indexOf("notifications.") === 0) {
        syncNotificationConfigValue(key, val);
      }
      sendChange(key, val);
    });

    valueEl.addEventListener("blur", function() {
      var val = clamp(valueEl.value);
      valueEl.value = String(val);
      if (key.indexOf("notifications.") === 0) {
        syncNotificationConfigValue(key, val);
      }
      sendChange(key, val);
    });

    valueEl.addEventListener("keydown", function(e) {
      if (e.key === "Enter") {
        e.preventDefault();
        valueEl.blur();
      }
    });
  });
}

// ─────────── Toggles ───────────

function initToggles() {
  document.querySelectorAll(".toggle").forEach(function(toggle) {
    const key = toggle.getAttribute("data-key");

    toggle.addEventListener("click", function() {
      const isOn = toggle.classList.contains("on");

      if (isOn) {
        toggle.classList.remove("on");
        toggle.classList.add("off");
      } else {
        toggle.classList.remove("off");
        toggle.classList.add("on");
      }

      sendChange(key, !isOn);
    });
  });
}

// ─────────── Segmented Controls ───────────

function initSegmented() {
  document.querySelectorAll(".segmented-control").forEach(function(ctrl) {
    const key = ctrl.getAttribute("data-key");
    const opts = ctrl.querySelectorAll(".segment-opt");

    opts.forEach(function(opt) {
      opt.addEventListener("click", function() {
        var value = opt.getAttribute("data-value");
        opts.forEach(function(o) { o.classList.remove("active"); });
        opt.classList.add("active");
        if (key.indexOf("notifications.") === 0) {
          syncNotificationConfigValue(key, value);
          if (key === "notifications.timeout_mode") {
            updateNotificationTimeoutVisibility();
          }
        }
        sendChange(key, value);
      });
    });
  });
}

// ─────────── Sliders ───────────

function initSliders() {
  document.querySelectorAll("input[type='range']").forEach(function(slider) {
    const key = slider.getAttribute("data-key");
    const display = slider.parentElement.querySelector(".slider-val");
    const precision = parseInt(slider.getAttribute("data-precision") || "1", 10);

    slider.addEventListener("input", function() {
      const val = parseFloat(slider.value);
      display.textContent = val.toFixed(precision);
    });

    slider.addEventListener("change", function() {
      sendChange(key, parseFloat(slider.value));
    });
  });
}

// ─────────── Select Dropdowns ───────────

function initSelects() {
  document.querySelectorAll("select.select-control").forEach(function(sel) {
    const key = sel.getAttribute("data-key");
    if (!key) { return; }

    sel.addEventListener("change", function() {
      sendChange(key, sel.value);
    });
  });
}

// ─────────── Text Inputs ───────────

function initTextInputs() {
  document.querySelectorAll("input.text-input").forEach(function(input) {
    const key = input.getAttribute("data-key");
    if (!key) { return; }

    input.addEventListener("change", function() {
      sendChange(key, input.value);
    });
  });
}

// ─────────── Color Swatches ───────────

function initColorSwatches() {
  document.querySelectorAll(".color-swatch").forEach(function(swatch) {
    const key = swatch.getAttribute("data-key");
    const colorInput = swatch.querySelector("input[type='color']");
    if (!colorInput || !key) { return; }

    colorInput.addEventListener("input", function() {
      swatch.style.background = colorInput.value;
    });

    colorInput.addEventListener("change", function() {
      sendChange(key, colorInput.value);
      if (key.startsWith("appearance.prompt_bar_")) {
        syncPromptBarConfigValue(key, colorInput.value);
      }
    });
  });

  document.querySelectorAll(".ansi-swatch").forEach(function(swatch) {
    const key = swatch.getAttribute("data-key");
    const colorInput = swatch.querySelector("input[type='color']");
    if (!colorInput || !key) { return; }

    colorInput.addEventListener("input", function() {
      swatch.style.background = colorInput.value;
    });

    colorInput.addEventListener("change", function() {
      sendChange(key, colorInput.value);
      if (key.startsWith("appearance.prompt_bar_")) {
        syncPromptBarConfigValue(key, colorInput.value);
      }
    });
  });
}

// ─────────── Workspace Roots ───────────

function initWorkspaces() {
  const list = document.getElementById("workspace-list");
  if (!list) { return; }

  list.addEventListener("click", function(e) {
    const removeBtn = e.target.closest(".workspace-remove");
    if (removeBtn) {
      const item = removeBtn.closest(".workspace-item");
      const path = item.querySelector(".workspace-path").textContent;
      item.remove();
      sendChange("workspaces.remove_root", path);
      return;
    }

    const addBtn = e.target.closest(".workspace-add");
    if (addBtn) {
      sendChange("workspaces.add_root", "");
    }
  });
}

// ─────────── Badge Colors ───────────

function populateBadgeColors(colors) {
  var list = document.getElementById("badge-color-list");
  if (!list) { return; }

  // Remove existing swatch groups but keep other siblings
  var existing = list.querySelectorAll(".color-swatch-group");
  existing.forEach(function(el) { el.remove(); });

  colors.forEach(function(color, index) {
    var key = "workspaces.badge_colors." + index;

    var group = document.createElement("div");
    group.className = "color-swatch-group";

    var swatch = document.createElement("div");
    swatch.className = "color-swatch";
    swatch.setAttribute("data-key", key);
    swatch.style.background = color;

    var colorInput = document.createElement("input");
    colorInput.type = "color";
    colorInput.value = color;

    colorInput.addEventListener("input", function() {
      swatch.style.background = colorInput.value;
    });

    colorInput.addEventListener("change", function() {
      sendChange(key, colorInput.value);
    });

    swatch.appendChild(colorInput);
    group.appendChild(swatch);

    var label = document.createElement("span");
    label.className = "color-swatch-label";
    label.textContent = String(index + 1);
    group.appendChild(label);

    list.appendChild(group);
  });
}

function initBadgeColorReset() {
  var btn = document.querySelector(".badge-color-reset");
  if (!btn) { return; }
  btn.addEventListener("click", function() {
    sendChange("workspaces.reset_badge_colors", true);
  });
}

// ─────────── loadConfig — called by Rust ───────────

function loadConfig(config) {
  currentConfig = config;

  // Appearance — Typography
  setSelectValue("appearance.font_family", config.appearance?.font);
  setStepperValue("appearance.font_size", config.appearance?.font_size);
  setStepperValue("appearance.font_weight", config.appearance?.font_weight);
  setStepperValue("appearance.bold_weight", config.appearance?.bold_weight);
  setToggleValue("appearance.ligatures", config.appearance?.ligatures);
  setStepperValue("appearance.line_padding", config.appearance?.line_padding);

  // Appearance — Cursor
  setSegmentedValue("appearance.cursor_shape", config.appearance?.cursor_shape);
  setToggleValue("appearance.cursor_blink", config.appearance?.cursor_blink);

  // Appearance — Window
  setSliderValue("appearance.opacity", config.appearance?.opacity);
  setStepperValue("appearance.tab_height", config.appearance?.tab_height);
  setStepperValue("appearance.tab_bar_padding", config.appearance?.tab_bar_padding);
  setStepperValue("appearance.status_bar_height", config.appearance?.status_bar_height);

  // Appearance — Focus Border
  setColorSwatch('appearance.focus_border_color', config.appearance?.focus_border_color || '#3b82f6');
  setStepperValue('appearance.focus_border_width', config.appearance?.focus_border_width ?? 2);

  // Terminal
  setStepperValue("terminal.scrollback_lines", config.terminal?.scrollback_lines);
  setToggleValue("terminal.natural_scroll", config.terminal?.natural_scroll);
  setToggleValue("terminal.copy_on_select", config.terminal?.copy_on_select);
  setToggleValue("terminal.claude_copy_cleanup", config.terminal?.claude_copy_cleanup);
  setToggleValue("terminal.claude_code_integration", config.terminal?.claude_code_integration);
  setToggleValue("terminal.codex_code_integration", config.terminal?.codex_code_integration);
  setToggleValue("terminal.auggie_integration", config.terminal?.auggie_integration);
  setToggleValue("terminal.preserve_ai_scrollback", config.terminal?.preserve_ai_scrollback);
  setToggleValue("terminal.prompt_bar", config.terminal?.prompt_bar);
  setToggleValue("terminal.scroll_pin", config.terminal?.scroll_pin);
  setSegmentedValue("terminal.prompt_bar_position", config.terminal?.prompt_bar_position || "top");
  setStepperValue("terminal.prompt_bar_font_size", config.terminal?.prompt_bar_font_size);
  setStepperValue("terminal.indicator_height", config.terminal?.indicator_height);
  smartSelectionState = normalizeSmartSelectionConfig(config.terminal?.smart_selection);
  smartSelectionSelectedIndex = 0;
  renderSmartSelection();

  // AI assistant states
  var states = config.terminal?.ai_states || config.terminal?.claude_states;
  if (states) {
    ["processing","idle_prompt","waiting_for_input","permission_prompt","error"]
      .forEach(function(s) {
        var e = states[s];
        if (!e) { return; }
        setToggleValue("ai_states." + s + ".tab_indicator", e.tab_indicator);
        setToggleValue("ai_states." + s + ".pane_border", e.pane_border);
        // Color swatches only accept hex; ansi:N values are left at default.
        if (typeof e.color === "string" && e.color.charAt(0) === "#") {
          setColorSwatch("ai_states." + s + ".color", e.color);
        }
        setStepperValue("ai_states." + s + ".pulse_ms", e.pulse_ms);
        setStepperValue("ai_states." + s + ".timeout_secs", e.timeout_secs);
      });
  }

  // Theme — appearance.theme is kebab-case, data-theme attrs use underscores
  var presetId = config.appearance?.theme;
  if (presetId) { presetId = presetId.replace(/-/g, "_"); }
  activeThemeId = presetId || "minimal_dark";
  isCustomMode = (presetId === "custom");

  if (isCustomMode && config.theme) {
    var tc = config.theme;
    populateColorEditor({
      fg: tc.foreground,
      bg: tc.background,
      cursor: tc.cursor,
      cursor_accent: tc.cursor_accent,
      selection: tc.selection,
      selection_fg: tc.selection_foreground,
      ansi: tc.colors || []
    });
  } else if (themeColors[activeThemeId]) {
    populateColorEditor(themeColors[activeThemeId]);
  }

  // Appearance — Prompt Bar
  syncPromptBarSwatches(config);

  renderThemeGrid();

  // Keybindings — values are now arrays of combo strings
  if (config.keybindings) {
    Object.keys(config.keybindings).forEach(function(action) {
      var val = config.keybindings[action];
      var list = Array.isArray(val) ? val : (val ? [val] : []);
      setKeybindingValue(action, list);
    });
  }

  // Workspaces
  if (config.workspaces?.roots) {
    populateWorkspaceRoots(config.workspaces.roots);
  }

  // Badge colors
  if (config.workspaces?.badge_colors) {
    populateBadgeColors(config.workspaces.badge_colors);
  }

  // Updates
  setToggleValue('update.enabled', config.update?.enabled ?? true);
  setStepperValue('update.check_interval_hours', Math.round((config.update?.check_interval_secs ?? 86400) / 3600));
  setSegmentedValue('update.channel', config.update?.channel ?? 'stable');

  // Notifications
  setToggleValue('notifications.enabled', config.notifications?.enabled ?? true);
  setSegmentedValue('notifications.condition', config.notifications?.condition ?? 'when_unfocused');
  setSegmentedValue('notifications.timeout_mode', config.notifications?.timeout_mode ?? 'system_default');
  setStepperValue('notifications.timeout_secs', config.notifications?.timeout_secs ?? 10);
  updateNotificationPlatformRows();
}

// ─────────── Value Setters ───────────

function setSelectValue(key, value) {
  if (value === undefined || value === null) { return; }
  var el = document.querySelector("select[data-key='" + key + "']");
  if (el) { el.value = value; }
}

function setStepperValue(key, value) {
  if (value === undefined || value === null) { return; }
  var ctrl = document.querySelector(".number-control[data-key='" + key + "']");
  if (ctrl) {
    var valEl = ctrl.querySelector(".number-value");
    if (valEl) { valEl.value = String(value); }
  }
}

function setToggleValue(key, value) {
  if (value === undefined || value === null) { return; }
  var toggle = document.querySelector(".toggle[data-key='" + key + "']");
  if (toggle) {
    toggle.classList.remove("on", "off");
    toggle.classList.add(value ? "on" : "off");
  }
}

function setSegmentedValue(key, value) {
  if (value === undefined || value === null) { return; }
  var ctrl = document.querySelector(".segmented-control[data-key='" + key + "']");
  if (ctrl) {
    ctrl.querySelectorAll(".segment-opt").forEach(function(opt) {
      if (opt.getAttribute("data-value") === value) {
        opt.classList.add("active");
      } else {
        opt.classList.remove("active");
      }
    });
  }
}

function setSliderValue(key, value) {
  if (value === undefined || value === null) { return; }
  var slider = document.querySelector("input[type='range'][data-key='" + key + "']");
  if (slider) {
    slider.value = value;
    var display = slider.parentElement.querySelector(".slider-val");
    var precision = parseInt(slider.getAttribute("data-precision") || "1", 10);
    if (display) { display.textContent = parseFloat(value).toFixed(precision); }
  }
}

function setTextValue(key, value) {
  if (value === undefined || value === null) { return; }
  var input = document.querySelector("input.text-input[data-key='" + key + "']");
  if (input) { input.value = value; }
}

function setColorSwatch(key, color) {
  if (!color) { return; }
  var swatch = document.querySelector(".color-swatch[data-key='" + key + "']");
  if (swatch) {
    swatch.style.background = color;
    var input = swatch.querySelector("input[type='color']");
    if (input) { input.value = color; }
  }
}

function syncPromptBarConfigValue(key, value) {
  if (!currentConfig.appearance) {
    currentConfig.appearance = {};
  }

  if (key === "appearance.prompt_bar_second_row_bg") {
    if (value === undefined || value === null || value === "") {
      delete currentConfig.appearance.prompt_bar_second_row_bg;
      delete currentConfig.appearance.prompt_bar_bg;
    } else {
      currentConfig.appearance.prompt_bar_second_row_bg = value;
      delete currentConfig.appearance.prompt_bar_bg;
    }
    return;
  }

  var appearanceKey = key.slice("appearance.".length);
  if (value === undefined || value === null || value === "") {
    delete currentConfig.appearance[appearanceKey];
  } else {
    currentConfig.appearance[appearanceKey] = value;
  }
}

function setColorSwatchVisual(key, swatchColor, inputColor) {
  if (!swatchColor) { return; }
  var swatch = document.querySelector(".color-swatch[data-key='" + key + "']");
  if (swatch) {
    swatch.style.background = swatchColor;
    var input = swatch.querySelector("input[type='color']");
    if (input && inputColor) { input.value = inputColor; }
  }
}

function setAnsiSwatch(key, color) {
  if (!color) { return; }
  var swatch = document.querySelector(".ansi-swatch[data-key='" + key + "']");
  if (swatch) {
    swatch.style.background = color;
    var input = swatch.querySelector("input[type='color']");
    if (input) { input.value = color; }
  }
}

var NAMED_KEYS = ["pageup", "pagedown", "home", "end", "left", "right", "up", "down",
  "tab", "backspace", "delete", "enter", "space", "escape"];

var MODIFIER_DISPLAY_MAC = { "cmd": "\u2318", "super": "\u2318", "ctrl": "\u2303", "shift": "\u21e7", "alt": "\u2325" };
var MODIFIER_DISPLAY_LINUX = { "cmd": "Super", "super": "Super", "ctrl": "Ctrl", "shift": "Shift", "alt": "Alt" };

function splitKeybindingParts(shortcut) {
  // Robust split: handle "+" as a key (e.g. "ctrl++").
  var parts = shortcut.split("+");
  parts = parts.filter(function(p) { return p !== ""; });
  if (shortcut.endsWith("+") && shortcut.length > 1) {
    parts.push("+");
  }
  return parts.map(function(part) { return part.trim(); });
}

function formatKeybinding(shortcut) {
  var isMac = window.SCRIBE_PLATFORM === "macos";
  var glyphs = isMac ? MODIFIER_DISPLAY_MAC : MODIFIER_DISPLAY_LINUX;

  var parts = splitKeybindingParts(shortcut).map(function(p) {
    if (glyphs[p] !== undefined) { return glyphs[p]; }
    if (p.length === 1) { return p.toUpperCase(); }
    if (NAMED_KEYS.indexOf(p) !== -1) { return p.charAt(0).toUpperCase() + p.slice(1); }
    return p;
  });
  return isMac ? parts.join("") : parts.join("+");
}

function keybindingSearchText(shortcut) {
  var isMac = window.SCRIBE_PLATFORM === "macos";
  var parts = splitKeybindingParts((shortcut || "").toLowerCase());
  var aliases = [(shortcut || "").toLowerCase()];

  parts.forEach(function(part) {
    if (!part) { return; }
    aliases.push(part);
    if (part === "cmd" || part === "super") {
      aliases.push("cmd", "command", "super", "meta");
    } else if (part === "ctrl") {
      aliases.push("ctrl", "control");
    } else if (part === "alt") {
      aliases.push("alt");
      if (isMac) { aliases.push("option"); }
    } else if (part === "shift") {
      aliases.push("shift");
    }
  });

  return Array.from(new Set(aliases)).join(" ");
}

var MAX_BINDINGS = 5;

function setKeybindingValue(action, combos) {
  var cell = document.querySelector(".keybinding-cell[data-action='" + action + "']");
  if (!cell) { return; }
  renderBadges(cell, action, combos);
}

function renderBadges(cell, action, combos) {
  // Remove existing badges, remove buttons, and add button — keep reset button.
  var toRemove = cell.querySelectorAll(".keybinding-key, .kb-remove-btn, .kb-add-btn");
  for (var i = 0; i < toRemove.length; i++) { toRemove[i].remove(); }

  var resetBtn = cell.querySelector(".kb-reset-btn");

  for (var idx = 0; idx < combos.length; idx++) {
    var badge = document.createElement("span");
    badge.className = "keybinding-key";
    badge.setAttribute("data-action", action);
    badge.setAttribute("data-index", String(idx));
    badge.setAttribute("data-current", combos[idx]);
    badge.setAttribute("data-search", keybindingSearchText(combos[idx]));
    badge.textContent = formatKeybinding(combos[idx]);
    cell.insertBefore(badge, resetBtn);

    var removeBtn = document.createElement("button");
    removeBtn.className = "kb-remove-btn";
    removeBtn.setAttribute("data-action", action);
    removeBtn.setAttribute("data-index", String(idx));
    removeBtn.title = "Remove binding";
    removeBtn.textContent = "\u00d7";
    cell.insertBefore(removeBtn, resetBtn);
  }

  if (combos.length < MAX_BINDINGS) {
    var addBtn = document.createElement("button");
    addBtn.className = "kb-add-btn";
    addBtn.setAttribute("data-action", action);
    addBtn.title = "Add binding";
    addBtn.textContent = "+";
    cell.insertBefore(addBtn, resetBtn);
  }

  // Re-apply search highlighting if a search query is active.
  var searchInput = document.getElementById("global-search");
  var query = searchInput ? searchInput.value.trim().toLowerCase() : "";
  if (query) {
    var badges = cell.querySelectorAll(".keybinding-key");
    for (var h = 0; h < badges.length; h++) {
      highlightKeybindingBadge(badges[h], query);
    }
  }
}

function getCombosForAction(action) {
  var badges = document.querySelectorAll(".keybinding-key[data-action='" + action + "']");
  var combos = [];
  for (var i = 0; i < badges.length; i++) {
    var c = badges[i].getAttribute("data-current");
    if (c) { combos.push(c); }
  }
  return combos;
}

function populateWorkspaceRoots(roots) {
  var list = document.getElementById("workspace-list");
  if (!list) { return; }

  // Remove existing items but keep the add button
  var addBtn = list.querySelector(".workspace-add");
  var items = list.querySelectorAll(".workspace-item");
  items.forEach(function(item) { item.remove(); });

  // Build items using safe DOM methods
  roots.forEach(function(path) {
    var item = document.createElement("div");
    item.className = "workspace-item";

    var pathSpan = document.createElement("span");
    pathSpan.className = "workspace-path";
    pathSpan.textContent = path;
    item.appendChild(pathSpan);

    var removeButton = document.createElement("button");
    removeButton.className = "workspace-remove";
    removeButton.title = "Remove";

    var svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
    svg.setAttribute("width", "14");
    svg.setAttribute("height", "14");
    svg.setAttribute("viewBox", "0 0 24 24");
    svg.setAttribute("fill", "none");
    svg.setAttribute("stroke", "currentColor");
    svg.setAttribute("stroke-width", "2");
    svg.setAttribute("stroke-linecap", "round");
    svg.setAttribute("stroke-linejoin", "round");

    var line1 = document.createElementNS("http://www.w3.org/2000/svg", "line");
    line1.setAttribute("x1", "18");
    line1.setAttribute("y1", "6");
    line1.setAttribute("x2", "6");
    line1.setAttribute("y2", "18");

    var line2 = document.createElementNS("http://www.w3.org/2000/svg", "line");
    line2.setAttribute("x1", "6");
    line2.setAttribute("y1", "6");
    line2.setAttribute("x2", "18");
    line2.setAttribute("y2", "18");

    svg.appendChild(line1);
    svg.appendChild(line2);
    removeButton.appendChild(svg);
    item.appendChild(removeButton);

    list.insertBefore(item, addBtn);
  });
}

// ─────────── Keybinding Recording ───────────

var KEY_NAMES = {
  " ": "space",
  "ArrowLeft": "left",
  "ArrowRight": "right",
  "ArrowUp": "up",
  "ArrowDown": "down",
  "PageUp": "pageup",
  "PageDown": "pagedown",
  "Home": "home",
  "End": "end",
  "Backspace": "backspace",
  "Delete": "delete",
  "Enter": "enter",
  "Tab": "tab",
  "Escape": "escape"
};

var MODIFIER_KEYS = ["Control", "Shift", "Alt", "Meta"];

function buildComboString(e) {
  // Ignore modifier-only keypresses.
  if (MODIFIER_KEYS.indexOf(e.key) !== -1) {
    return null;
  }

  var parts = [];
  if (e.metaKey) { parts.push("cmd"); }
  if (e.ctrlKey) { parts.push("ctrl"); }
  if (e.shiftKey) { parts.push("shift"); }
  if (e.altKey) { parts.push("alt"); }

  var keyName = KEY_NAMES[e.key];
  if (!keyName) {
    if (e.key.length === 1) {
      keyName = e.key.toLowerCase();
    } else {
      return null;
    }
  }

  parts.push(keyName);
  return parts.join("+");
}

function startRecording(el) {
  // Cancel any active recording first.
  if (recordingEl) {
    cancelRecording();
  }

  recordingPrev = el.getAttribute("data-current") || "";
  recordingPrevText = el.textContent;
  el.classList.add("recording");
  el.textContent = "Press keys...";
  recordingEl = el;
}

function cancelRecording() {
  if (!recordingEl) { return; }
  var action = recordingEl.getAttribute("data-action");
  var isNew = !recordingPrev;
  recordingEl.classList.remove("recording");

  if (isNew) {
    // Was a newly added placeholder — remove it and re-render.
    var combos = getCombosForAction(action).filter(function(c) { return c; });
    var cell = document.querySelector(".keybinding-cell[data-action='" + action + "']");
    recordingEl = null;
    recordingPrev = null;
    recordingPrevText = null;
    if (cell) { renderBadges(cell, action, combos); }
  } else {
    recordingEl.textContent = formatKeybinding(recordingPrev);
    recordingEl = null;
    recordingPrev = null;
    recordingPrevText = null;
  }
}

function finishRecording(combo) {
  if (!recordingEl) { return; }
  var action = recordingEl.getAttribute("data-action");
  var idx = parseInt(recordingEl.getAttribute("data-index"), 10);

  // Collect current combos and update the recorded index.
  var combos = getCombosForAction(action);
  if (idx < combos.length) {
    combos[idx] = combo;
  } else {
    combos.push(combo);
  }

  recordingEl.classList.remove("recording");
  recordingEl = null;
  recordingPrev = null;
  recordingPrevText = null;

  // Re-render and persist.
  var cell = document.querySelector(".keybinding-cell[data-action='" + action + "']");
  if (cell) { renderBadges(cell, action, combos); }
  sendChange("keybindings." + action, combos);
}

function removeKeybinding(action, idx) {
  var combos = getCombosForAction(action);
  combos.splice(idx, 1);
  var cell = document.querySelector(".keybinding-cell[data-action='" + action + "']");
  if (cell) { renderBadges(cell, action, combos); }
  sendChange("keybindings." + action, combos);
  hideConflictWarning();
}

function addKeybinding(action) {
  var combos = getCombosForAction(action);
  if (combos.length >= MAX_BINDINGS) { return; }

  var cell = document.querySelector(".keybinding-cell[data-action='" + action + "']");
  if (!cell) { return; }

  // Create a placeholder badge at the next index and start recording on it.
  var newIdx = combos.length;
  var badge = document.createElement("span");
  badge.className = "keybinding-key";
  badge.setAttribute("data-action", action);
  badge.setAttribute("data-index", String(newIdx));
  badge.setAttribute("data-current", "");

  // Insert before the add button (or reset button).
  var addBtn = cell.querySelector(".kb-add-btn");
  var resetBtn = cell.querySelector(".kb-reset-btn");
  cell.insertBefore(badge, addBtn || resetBtn);

  // Remove the add button while recording.
  if (addBtn) { addBtn.remove(); }

  startRecording(badge);
}

function findConflict(sourceAction, combo) {
  var normalized = combo.toLowerCase();
  var all = document.querySelectorAll(".keybinding-key[data-action]");
  for (var i = 0; i < all.length; i++) {
    var el = all[i];
    // Skip all badges of the same action (multi-bind for same action is fine).
    if (el.getAttribute("data-action") === sourceAction) { continue; }
    var current = (el.getAttribute("data-current") || "").toLowerCase();
    if (current === normalized) {
      return el.getAttribute("data-action");
    }
  }
  return null;
}

function showConflictWarning(conflictAction, combo) {
  var banner = document.getElementById("kb-conflict-banner");
  if (!banner) { return; }
  var label = conflictAction.replace(/_/g, " ");
  banner.textContent = formatKeybinding(combo) + " is already assigned to " + label;
  banner.style.display = "block";
}

function hideConflictWarning() {
  var banner = document.getElementById("kb-conflict-banner");
  if (banner) { banner.style.display = "none"; }
}

function resetKeybinding(action) {
  var def = keybindingDefaults[action];
  if (!def) { return; }
  // Defaults are now arrays; get the first default combo.
  var firstDefault = Array.isArray(def) ? def[0] : def;
  if (!firstDefault) { return; }

  var combos = getCombosForAction(action);
  if (combos.length === 0) {
    combos = [firstDefault];
  } else {
    combos[0] = firstDefault;
  }

  var cell = document.querySelector(".keybinding-cell[data-action='" + action + "']");
  if (cell) { renderBadges(cell, action, combos); }
  sendChange("keybindings." + action, combos);
  hideConflictWarning();
}

function initKeybindingRecorder() {
  // Click delegation for keybinding badges, add, remove, and reset.
  var page = document.getElementById("page-keybindings");
  if (!page) { return; }

  page.addEventListener("click", function(e) {
    var badge = e.target.closest(".keybinding-key");
    if (badge && !badge.classList.contains("kb-add-btn")) {
      e.stopPropagation();
      startRecording(badge);
      return;
    }

    var addBtn = e.target.closest(".kb-add-btn");
    if (addBtn) {
      e.stopPropagation();
      var addAction = addBtn.getAttribute("data-action");
      if (addAction) { addKeybinding(addAction); }
      return;
    }

    var removeBtn = e.target.closest(".kb-remove-btn");
    if (removeBtn) {
      e.stopPropagation();
      var rmAction = removeBtn.getAttribute("data-action");
      var rmIdx = parseInt(removeBtn.getAttribute("data-index"), 10);
      if (rmAction) { removeKeybinding(rmAction, rmIdx); }
      return;
    }

    var resetBtn = e.target.closest(".kb-reset-btn");
    if (resetBtn) {
      e.stopPropagation();
      var resetAction = resetBtn.getAttribute("data-action");
      if (resetAction) { resetKeybinding(resetAction); }
    }
  });

  // Global keydown listener for recording (capture phase).
  document.addEventListener("keydown", function(e) {
    if (!recordingEl) { return; }
    e.preventDefault();
    e.stopPropagation();

    if (e.key === "Escape") {
      cancelRecording();
      hideConflictWarning();
      return;
    }

    var combo = buildComboString(e);
    if (!combo) { return; }

    var action = recordingEl.getAttribute("data-action");
    var conflict = findConflict(action, combo);

    if (conflict) {
      showConflictWarning(conflict, combo);
    } else {
      hideConflictWarning();
    }

    finishRecording(combo);
  }, true);
}

// ─────────── Search ───────────

function clearHighlights(container) {
  var marks = container.querySelectorAll(".search-highlight");
  for (var i = 0; i < marks.length; i++) {
    var mark = marks[i];
    var parent = mark.parentNode;
    parent.replaceChild(document.createTextNode(mark.textContent), mark);
    parent.normalize();
  }

  var aliasMatches = container.querySelectorAll(".search-alias-match");
  for (var j = 0; j < aliasMatches.length; j++) {
    aliasMatches[j].classList.remove("search-alias-match");
  }
}

function highlightText(el, query) {
  if (!query || !el) { return; }
  var text = el.textContent;
  var lower = text.toLowerCase();
  var idx = lower.indexOf(query);
  if (idx === -1) { return; }

  var before = document.createTextNode(text.slice(0, idx));
  var mark = document.createElement("span");
  mark.className = "search-highlight";
  mark.textContent = text.slice(idx, idx + query.length);
  var after = document.createTextNode(text.slice(idx + query.length));

  el.textContent = "";
  el.appendChild(before);
  el.appendChild(mark);
  el.appendChild(after);
}

function highlightKeybindingBadge(el, query) {
  if (!query || !el) { return; }
  el.classList.remove("search-alias-match");
  if (el.textContent.toLowerCase().indexOf(query) !== -1) {
    highlightText(el, query);
    return;
  }
  var searchText = (el.getAttribute("data-search") || "").toLowerCase();
  if (searchText.indexOf(query) !== -1) {
    el.classList.add("search-alias-match");
  }
}

// ─────────── Global Search ───────────

var activeTabBeforeSearch = null;

function initGlobalSearch() {
  var input = document.getElementById("global-search");
  if (!input) { return; }

  var wrapper = input.closest(".global-search-wrapper");
  var clearBtn = wrapper ? wrapper.querySelector(".global-search-clear") : null;

  function updateClearVisibility() {
    if (wrapper) {
      if (input.value.length > 0) {
        wrapper.classList.add("has-value");
      } else {
        wrapper.classList.remove("has-value");
      }
    }
  }

  function clearSearch() {
    input.value = "";
    updateClearVisibility();
    filterAllSettings("");
    input.focus();
  }

  input.addEventListener("input", function() {
    var query = input.value.trim().toLowerCase();
    updateClearVisibility();
    filterAllSettings(query);
  });

  input.addEventListener("keydown", function(e) {
    if (e.key === "Escape") {
      e.preventDefault();
      clearSearch();
    }
  });

  if (clearBtn) {
    clearBtn.addEventListener("click", function() {
      clearSearch();
    });
  }
}

function filterAllSettings(query) {
  var pages = document.querySelectorAll(".content-page");
  var navItems = document.querySelectorAll(".nav-item");

  // Clear all highlights across all pages.
  pages.forEach(function(page) { clearHighlights(page); });

  if (!query) {
    // Restore the page that was active before search began.
    pages.forEach(function(page) { page.style.display = ""; });
    navItems.forEach(function(n) { n.classList.remove("active"); });

    var restoreTab = activeTabBeforeSearch || (navItems.length > 0 ? navItems[0].getAttribute("data-tab") : null);
    activeTabBeforeSearch = null;

    pages.forEach(function(page) {
      if (page.id === "page-" + restoreTab) {
        page.classList.add("active");
      } else {
        page.classList.remove("active");
      }
    });
    navItems.forEach(function(n) {
      if (n.getAttribute("data-tab") === restoreTab) {
        n.classList.add("active");
      }
    });

    // Restore all section-groups and setting-rows.
    document.querySelectorAll(".section-group").forEach(function(g) { g.style.display = ""; });
    document.querySelectorAll(".setting-row").forEach(function(r) { r.style.display = ""; });
    return;
  }

  // Save the currently active tab before entering search mode.
  if (activeTabBeforeSearch === null) {
    var activeNav = document.querySelector(".nav-item.active");
    activeTabBeforeSearch = activeNav ? activeNav.getAttribute("data-tab") : null;
  }

  var firstMatchingTab = null;

  pages.forEach(function(page) {
    var pageHasMatch = false;
    var groups = page.querySelectorAll(".section-group");

    groups.forEach(function(group) {
      var sectionLabel = group.querySelector(".section-label");
      var sectionText = sectionLabel ? sectionLabel.textContent.toLowerCase() : "";
      var sectionMatches = sectionText.indexOf(query) !== -1;
      var rows = group.querySelectorAll(".setting-row");
      var anyRowVisible = false;

      if (sectionMatches) {
        // Whole section matches — show all rows.
        rows.forEach(function(row) { row.style.display = ""; });
        anyRowVisible = true;
        if (sectionLabel) { highlightText(sectionLabel, query); }
      } else {
        rows.forEach(function(row) {
          var labelEl = row.querySelector(".setting-label");
          var descEl = row.querySelector(".setting-desc");
          var keyEls = row.querySelectorAll(".keybinding-key");
          var themeNameEl = row.querySelector(".theme-name span");
          var workspacePathEl = row.querySelector(".workspace-path");
          var colorSwatchLabelEls = row.querySelectorAll(".color-swatch-label");

          var labelText = labelEl ? labelEl.textContent.toLowerCase() : "";
          var descText = descEl ? descEl.textContent.toLowerCase() : "";
          var themeText = themeNameEl ? themeNameEl.textContent.toLowerCase() : "";
          var workspaceText = workspacePathEl ? workspacePathEl.textContent.toLowerCase() : "";
          var keyText = "";
          for (var k = 0; k < keyEls.length; k++) {
            keyText += " " + ((keyEls[k].getAttribute("data-search") || keyEls[k].textContent).toLowerCase());
          }
          var swatchText = "";
          for (var s = 0; s < colorSwatchLabelEls.length; s++) {
            swatchText += " " + colorSwatchLabelEls[s].textContent.toLowerCase();
          }

          var matches =
            labelText.indexOf(query) !== -1 ||
            descText.indexOf(query) !== -1 ||
            keyText.indexOf(query) !== -1 ||
            themeText.indexOf(query) !== -1 ||
            workspaceText.indexOf(query) !== -1 ||
            swatchText.indexOf(query) !== -1;

          row.style.display = matches ? "" : "none";

          if (matches) {
            anyRowVisible = true;
            if (labelEl) { highlightText(labelEl, query); }
            if (descEl) { highlightText(descEl, query); }
            if (themeNameEl) { highlightText(themeNameEl, query); }
            if (workspacePathEl) { highlightText(workspacePathEl, query); }
            for (var k2 = 0; k2 < keyEls.length; k2++) { highlightKeybindingBadge(keyEls[k2], query); }
            for (var s2 = 0; s2 < colorSwatchLabelEls.length; s2++) { highlightText(colorSwatchLabelEls[s2], query); }
          }
        });

        // Also check theme cards outside setting-rows (theme page preset grid).
        if (!anyRowVisible) {
          var themeCards = group.querySelectorAll(".theme-card");
          var anyCardVisible = false;
          themeCards.forEach(function(card) {
            var nameEl = card.querySelector(".theme-name span");
            if (nameEl && nameEl.textContent.toLowerCase().indexOf(query) !== -1) {
              anyCardVisible = true;
            }
          });
          if (anyCardVisible) { anyRowVisible = true; }
        }
      }

      group.style.display = anyRowVisible ? "" : "none";
      if (anyRowVisible) { pageHasMatch = true; }
    });

    // Show/hide the page based on whether it has matches.
    if (pageHasMatch) {
      page.style.display = "";
      if (!firstMatchingTab) {
        var tabId = page.id.replace("page-", "");
        firstMatchingTab = tabId;
      }
    } else {
      page.style.display = "none";
    }
  });

  // Update sidebar to show the first matching page as active.
  navItems.forEach(function(n) { n.classList.remove("active"); });
  pages.forEach(function(p) { p.classList.remove("active"); });

  if (firstMatchingTab) {
    navItems.forEach(function(n) {
      if (n.getAttribute("data-tab") === firstMatchingTab) {
        n.classList.add("active");
      }
    });
    var firstPage = document.getElementById("page-" + firstMatchingTab);
    if (firstPage) { firstPage.classList.add("active"); }
  }
}

// ─────────── Init ───────────

document.addEventListener("DOMContentLoaded", function() {
  // Sidebar footer: write the running Scribe version sourced from the host
  // bootstrap script (set by lib.rs via WebViewBuilder::with_initialization_script
  // before page load). Falls back to plain "Scribe" when the value is missing.
  var footerEl = document.getElementById("sidebar-footer");
  if (footerEl) {
    var bootstrapVersion = window.SCRIBE_BOOTSTRAP && window.SCRIBE_BOOTSTRAP.version;
    footerEl.textContent = bootstrapVersion ? "Scribe v" + bootstrapVersion : "Scribe";
  }

  updateNotificationPlatformRows();
  initNavigation();
  initSteppers();
  initToggles();
  initSegmented();
  initSliders();
  initSelects();
  initTextInputs();
  initColorSwatches();
  initSmartSelection();
  initThemeColorEditor();

  // Reset-to-default buttons
  document.querySelectorAll('.reset-btn').forEach(btn => {
    btn.addEventListener('click', () => {
      const key = btn.dataset.resetKey;
      if (!key) return;
      if (key === 'appearance.focus_border_color') {
        // Send empty string to clear override (server sets to None)
        sendChange(key, '');
        // Reset swatch to theme default visually (ANSI blue)
        const swatch = document.querySelector(`.color-swatch[data-key="${key}"]`);
        if (swatch) {
          swatch.style.background = '#3b82f6';
          const input = swatch.querySelector('input[type="color"]');
          if (input) input.value = '#3b82f6';
        }
      } else if (key === 'appearance.focus_border_width') {
        sendChange(key, 2);
        setStepperValue(key, 2);
      } else if (key.startsWith('appearance.prompt_bar_')) {
        // Send empty string to clear override (server sets to None)
        sendChange(key, '');
        syncPromptBarConfigValue(key, '');
        // Reset swatch to default visual
        const promptBarDefaults = getPromptBarThemeDefaults();
        const defaults = {
          'appearance.prompt_bar_second_row_bg': promptBarDefaults.secondRow,
          'appearance.prompt_bar_first_row_bg': promptBarDefaults.firstRow,
          'appearance.prompt_bar_text': promptBarDefaults.textSwatch,
          'appearance.prompt_bar_icon_first': promptBarDefaults.iconFirst,
          'appearance.prompt_bar_icon_latest': promptBarDefaults.iconLatest
        };
        const def = defaults[key];
        if (def) {
          if (key === 'appearance.prompt_bar_text') {
            setColorSwatchVisual(key, def, promptBarDefaults.textInput);
          } else {
            setColorSwatchVisual(key, def, def);
          }
        }
      }
    });
  });

  initWorkspaces();
  initBadgeColorReset();
  initNotificationActions();
  initUpdateActions();
  initReleasesPanel();
  initGlobalSearch();
  initKeybindingRecorder();

  // Font refresh button.
  const refreshBtn = document.getElementById("refresh-fonts");
  if (refreshBtn) {
    refreshBtn.addEventListener("click", requestFontRefresh);
  }
});

// Scribe Settings UI — JavaScript

// ─────────── IPC ───────────

function sendChange(key, value) {
  if (window.ipc && window.ipc.postMessage) {
    window.ipc.postMessage(JSON.stringify({ type: "setting_changed", key, value }));
  }
}

// ─────────── State ───────────

let currentConfig = {};
let keybindingDefaults = {};
let recordingEl = null;
let recordingPrev = null;
let recordingPrevText = null;

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
  if (window.ipc && window.ipc.postMessage) {
    window.ipc.postMessage(JSON.stringify({ type: "request_fonts" }));
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
    });
  });
}

// ─────────── Number Steppers ───────────

function initSteppers() {
  document.querySelectorAll(".number-control").forEach(function(ctrl) {
    const key = ctrl.getAttribute("data-key");
    const step = parseFloat(ctrl.getAttribute("data-step") || "1");
    const min = parseFloat(ctrl.getAttribute("data-min") || "0");
    const max = parseFloat(ctrl.getAttribute("data-max") || "99999");
    const valueEl = ctrl.querySelector(".number-value");
    const btns = ctrl.querySelectorAll(".number-btn");

    btns[0].addEventListener("click", function() {
      let val = parseFloat(valueEl.textContent) - step;
      if (val < min) { val = min; }
      valueEl.textContent = String(val);
      sendChange(key, val);
    });

    btns[1].addEventListener("click", function() {
      let val = parseFloat(valueEl.textContent) + step;
      if (val > max) { val = max; }
      valueEl.textContent = String(val);
      sendChange(key, val);
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
        opts.forEach(function(o) { o.classList.remove("active"); });
        opt.classList.add("active");
        sendChange(key, opt.getAttribute("data-value"));
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

    sel.addEventListener("change", function() {
      sendChange(key, sel.value);
    });
  });
}

// ─────────── Text Inputs ───────────

function initTextInputs() {
  document.querySelectorAll("input.text-input").forEach(function(input) {
    const key = input.getAttribute("data-key");

    input.addEventListener("change", function() {
      sendChange(key, input.value);
    });
  });
}

// ─────────── Theme Cards ───────────

function initThemeCards() {
  const cards = document.querySelectorAll(".theme-card");

  cards.forEach(function(card) {
    card.addEventListener("click", function() {
      cards.forEach(function(c) { c.classList.remove("selected"); });
      card.classList.add("selected");

      // Show/hide checkmark
      cards.forEach(function(c) {
        const check = c.querySelector(".theme-check");
        if (check) { check.style.display = "none"; }
      });
      const check = card.querySelector(".theme-check");
      if (check) { check.style.display = "flex"; }

      const themeName = card.getAttribute("data-theme");
      sendChange("theme.preset", themeName);
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
      swatch.style.backgroundColor = colorInput.value;
    });

    colorInput.addEventListener("change", function() {
      sendChange(key, colorInput.value);
    });
  });

  document.querySelectorAll(".ansi-swatch").forEach(function(swatch) {
    const key = swatch.getAttribute("data-key");
    const colorInput = swatch.querySelector("input[type='color']");
    if (!colorInput || !key) { return; }

    colorInput.addEventListener("input", function() {
      swatch.style.backgroundColor = colorInput.value;
    });

    colorInput.addEventListener("change", function() {
      sendChange(key, colorInput.value);
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

  // Terminal
  setStepperValue("terminal.scrollback_lines", config.terminal?.scrollback_lines);
  setToggleValue("terminal.copy_on_select", config.terminal?.copy_on_select);
  setToggleValue("terminal.claude_copy_cleanup", config.terminal?.claude_copy_cleanup);
  setToggleValue("terminal.claude_code_integration", config.terminal?.claude_code_integration);

  // Theme
  setThemePreset(config.theme?.preset);
  setColorSwatch("theme.foreground", config.theme?.foreground);
  setColorSwatch("theme.background", config.theme?.background);
  setColorSwatch("theme.cursor", config.theme?.cursor);
  setColorSwatch("theme.cursor_text", config.theme?.cursor_text);
  setColorSwatch("theme.selection", config.theme?.selection);
  setColorSwatch("theme.selection_text", config.theme?.selection_text);

  // ANSI colors
  if (config.theme?.ansi_normal) {
    config.theme.ansi_normal.forEach(function(color, i) {
      setAnsiSwatch("theme.ansi_normal." + i, color);
    });
  }
  if (config.theme?.ansi_bright) {
    config.theme.ansi_bright.forEach(function(color, i) {
      setAnsiSwatch("theme.ansi_bright." + i, color);
    });
  }

  // Keybindings
  if (config.keybindings) {
    Object.keys(config.keybindings).forEach(function(action) {
      setKeybindingValue(action, config.keybindings[action]);
    });
  }

  // Workspaces
  if (config.workspaces?.roots) {
    populateWorkspaceRoots(config.workspaces.roots);
  }
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
    if (valEl) { valEl.textContent = String(value); }
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

function setThemePreset(preset) {
  if (!preset) { return; }
  var cards = document.querySelectorAll(".theme-card");
  cards.forEach(function(card) {
    var check = card.querySelector(".theme-check");
    if (card.getAttribute("data-theme") === preset) {
      card.classList.add("selected");
      if (check) { check.style.display = "flex"; }
    } else {
      card.classList.remove("selected");
      if (check) { check.style.display = "none"; }
    }
  });
}

function setColorSwatch(key, color) {
  if (!color) { return; }
  var swatch = document.querySelector(".color-swatch[data-key='" + key + "']");
  if (swatch) {
    swatch.style.backgroundColor = color;
    var input = swatch.querySelector("input[type='color']");
    if (input) { input.value = color; }
  }
}

function setAnsiSwatch(key, color) {
  if (!color) { return; }
  var swatch = document.querySelector(".ansi-swatch[data-key='" + key + "']");
  if (swatch) {
    swatch.style.backgroundColor = color;
    var input = swatch.querySelector("input[type='color']");
    if (input) { input.value = color; }
  }
}

function formatKeybinding(shortcut) {
  return shortcut.split("+").map(function(part) {
    var p = part.trim();
    if (p === "ctrl" || p === "shift" || p === "alt") {
      return p.charAt(0).toUpperCase() + p.slice(1);
    }
    return p.length === 1 ? p.toUpperCase() : p;
  }).join("+");
}

function setKeybindingValue(action, shortcut) {
  var el = document.querySelector(".keybinding-key[data-action='" + action + "']");
  if (el) {
    el.textContent = formatKeybinding(shortcut);
    el.setAttribute("data-current", shortcut);
  }
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
  if (e.ctrlKey || e.metaKey) { parts.push("ctrl"); }
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
  // Use saved textContent as fallback in case data-current was empty.
  if (recordingPrev) {
    recordingEl.textContent = formatKeybinding(recordingPrev);
  } else {
    recordingEl.textContent = recordingPrevText || "";
  }
  recordingEl.classList.remove("recording");
  recordingEl = null;
  recordingPrev = null;
  recordingPrevText = null;
}

function finishRecording(combo) {
  if (!recordingEl) { return; }
  var action = recordingEl.getAttribute("data-action");

  recordingEl.setAttribute("data-current", combo);
  recordingEl.textContent = formatKeybinding(combo);
  recordingEl.classList.remove("recording");
  recordingEl = null;
  recordingPrev = null;
  recordingPrevText = null;

  sendChange("keybindings." + action, combo);
}

function findConflict(sourceAction, combo) {
  var normalized = combo.toLowerCase();
  var all = document.querySelectorAll(".keybinding-key[data-action]");
  for (var i = 0; i < all.length; i++) {
    var el = all[i];
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
  var el = document.querySelector(".keybinding-key[data-action='" + action + "']");
  if (!el) { return; }
  var def = keybindingDefaults[action];
  if (!def) { return; }
  el.setAttribute("data-current", def);
  el.textContent = formatKeybinding(def);
  sendChange("keybindings." + action, def);
  hideConflictWarning();
}

function initKeybindingRecorder() {
  // Click delegation for keybinding badges.
  var page = document.getElementById("page-keybindings");
  if (!page) { return; }

  page.addEventListener("click", function(e) {
    var badge = e.target.closest(".keybinding-key");
    if (badge) {
      e.stopPropagation();
      startRecording(badge);
      return;
    }

    var resetBtn = e.target.closest(".kb-reset-btn");
    if (resetBtn) {
      e.stopPropagation();
      var action = resetBtn.getAttribute("data-action");
      if (action) { resetKeybinding(action); }
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

// ─────────── Keybinding Search ───────────

function initKeybindingSearch() {
  var input = document.getElementById("kb-search");
  if (!input) { return; }

  input.addEventListener("input", function() {
    var query = input.value.trim().toLowerCase();
    filterKeybindingRows(query);
  });
}

function clearHighlights(container) {
  var marks = container.querySelectorAll(".kb-highlight");
  for (var i = 0; i < marks.length; i++) {
    var mark = marks[i];
    var parent = mark.parentNode;
    parent.replaceChild(document.createTextNode(mark.textContent), mark);
    parent.normalize();
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
  mark.className = "kb-highlight";
  mark.textContent = text.slice(idx, idx + query.length);
  var after = document.createTextNode(text.slice(idx + query.length));

  el.textContent = "";
  el.appendChild(before);
  el.appendChild(mark);
  el.appendChild(after);
}

function filterKeybindingRows(query) {
  var page = document.getElementById("page-keybindings");
  if (page) { clearHighlights(page); }

  var groups = document.querySelectorAll("#page-keybindings .section-group");
  groups.forEach(function(group) {
    var sectionLabel = group.querySelector(".section-label");
    var sectionText = sectionLabel ? sectionLabel.textContent.toLowerCase() : "";
    var sectionMatches = query && sectionText.indexOf(query) !== -1;
    var rows = group.querySelectorAll(".setting-row");
    var anyVisible = false;

    if (sectionMatches) {
      // Section header matches — show all rows in this section.
      rows.forEach(function(row) { row.style.display = ""; });
      anyVisible = true;
      if (query && sectionLabel) { highlightText(sectionLabel, query); }
    } else {
      rows.forEach(function(row) {
        var labelEl = row.querySelector(".setting-label");
        var keyEl = row.querySelector(".keybinding-key");
        var label = labelEl ? labelEl.textContent : "";
        var key = keyEl ? keyEl.textContent : "";
        var matches = !query ||
          label.toLowerCase().indexOf(query) !== -1 ||
          key.toLowerCase().indexOf(query) !== -1;

        row.style.display = matches ? "" : "none";
        if (matches && query) {
          highlightText(labelEl, query);
          highlightText(keyEl, query);
          anyVisible = true;
        } else if (matches) {
          anyVisible = true;
        }
      });
    }

    group.style.display = anyVisible ? "" : "none";
  });
}

// ─────────── Init ───────────

document.addEventListener("DOMContentLoaded", function() {
  initNavigation();
  initSteppers();
  initToggles();
  initSegmented();
  initSliders();
  initSelects();
  initTextInputs();
  initThemeCards();
  initColorSwatches();
  initWorkspaces();
  initKeybindingSearch();
  initKeybindingRecorder();

  // Font refresh button.
  const refreshBtn = document.getElementById("refresh-fonts");
  if (refreshBtn) {
    refreshBtn.addEventListener("click", requestFontRefresh);
  }
});

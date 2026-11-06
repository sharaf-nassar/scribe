// SCRIBE-MANAGED-PI-EXTENSION
// Scribe lifecycle adapter for Pi. Installed at user scope by Scribe.

import { type ChildProcess, spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

const REGISTRATION = Symbol.for("scribe.pi.lifecycle-extension");
const MAX_OUTSTANDING = 32;
const HELPER_TIMEOUT_MS = 100;
const TASK_LABEL_LIMIT = 120;
const ASK_USER_BLOCKED_EVENT = "rpiv:ask-user:blocked";
// pi-background-tasks' EventBus API v1 (its docs/api/eventbus-v1.md).
const BG_REQUEST_CHANNEL = "pi-background-tasks:request:v1";
const BG_RESPONSE_CHANNEL = "pi-background-tasks:response:v1";
const BG_TERMINAL_CHANNEL = "pi-background-tasks:terminal:v1";

type EventName =
  | "state_changed"
  | "session_stopped"
  | "state_cleared"
  | "prompt_received"
  | "task_label_changed"
  | "task_label_cleared"
  | "context_changed"
  | "issue_focused";

type Payload = Record<string, string | number>;
type QueuedEvent = { event: EventName; payload: Payload; generation: number };

function taskLabel(prompt: string): string {
  for (const rawLine of prompt.split(/\r?\n/)) {
    const line = rawLine.trim();
    if (!line || line.startsWith("/")) continue;
    const normalized = line
      .replace(/\p{C}/gu, " ")
      .replaceAll(";", ",")
      .replace(/\s+/g, " ")
      .trim();
    return Array.from(normalized).slice(0, TASK_LABEL_LIMIT).join("");
  }
  return "";
}

// Shell separators that end one simple command. Splitting on these keeps a
// claim buried in a chain (`cd x && bd update id --claim`) observable without
// pulling in a real shell parser.
const COMMAND_SEPARATORS = /\r?\n|;|&&|\|\||[|&]/;

// Tracker ids are lowercase, hyphenated, with an optional dotted child suffix:
// `scribe-lpi2`, `scribe-lpi2.13`, `bd-42`. Anchored, so a generated assignee
// slug like `codex-implement-ready-run-20260818T085731.REeEi4` cannot match.
const ISSUE_ID = /^[a-z][a-z0-9]*(?:-[a-z0-9]+)+(?:\.\d+)*$/;

/**
 * Extract the issue id from a bash command that claims a Beads issue, or
 * `undefined` when the command claims nothing.
 *
 * Conservative by design: a false positive pins a halo to the wrong issue,
 * which is worse than showing none at all, so every condition must hold — the
 * segment's command word is `bd`, `--claim` is present as its own token, and
 * the id is a positional rather than some flag's value.
 */
function claimedIssueId(command: string): string | undefined {
  for (const segment of command.split(COMMAND_SEPARATORS)) {
    const tokens = segment.trim().split(/\s+/).filter(Boolean);
    // Skip leading `VAR=value` prefixes so `BD_NO_DAEMON=1 bd …` still counts.
    let start = 0;
    while (tokens[start] && /^[A-Za-z_][A-Za-z0-9_]*=/.test(tokens[start])) start += 1;
    const argv0 = tokens[start];
    if (!argv0 || (argv0 !== "bd" && !argv0.endsWith("/bd"))) continue;
    const args = tokens.slice(start + 1);
    if (!args.includes("--claim")) continue;
    for (let index = 0; index < args.length; index += 1) {
      const token = args[index];
      if (token.startsWith("-")) continue;
      // A bare value belonging to a preceding `--flag value` pair is not a
      // positional id (`--actor someone` must never be read as one).
      const previous = args[index - 1];
      if (previous?.startsWith("-") && !previous.includes("=")) continue;
      if (ISSUE_ID.test(token)) return token;
    }
  }
  return undefined;
}

function assistantText(message: unknown): string | undefined {
  if (!message || typeof message !== "object") return undefined;
  const candidate = message as { role?: unknown; content?: unknown };
  if (candidate.role !== "assistant" || !Array.isArray(candidate.content)) return undefined;
  return candidate.content
    .filter(
      (part): part is { type: "text"; text: string } =>
        !!part &&
        typeof part === "object" &&
        (part as { type?: unknown }).type === "text" &&
        typeof (part as { text?: unknown }).text === "string",
    )
    .map((part) => part.text)
    .join("\n");
}

type RunningTask = { id: string; notifyOnCompletion?: unknown; triggerOnCompletion?: unknown };

/** Whether a pi-background-tasks snapshot is work that is still running. */
function runningTask(task: unknown): task is RunningTask {
  const snapshot = task as { id?: unknown; status?: unknown } | null;
  return typeof snapshot?.id === "string" && snapshot.status === "running";
}

/** Whether a task's completion notification will start another turn of this agent. */
function wakesAgent(task: RunningTask) {
  return task.notifyOnCompletion === true && task.triggerOnCompletion === true;
}

function contextPercent(ctx: { getContextUsage(): { percent: number | null } | undefined }) {
  try {
    const percent = ctx.getContextUsage()?.percent;
    if (typeof percent !== "number" || !Number.isFinite(percent)) return undefined;
    return Math.max(0, Math.min(100, Math.round(percent)));
  } catch {
    return undefined;
  }
}

// ── Scribe agent API tools ──────────────────────────────────────────
// Typed wrappers over the packaged `scribe agent` CLI (spec 027): Pi gets
// tools instead of a prose skill file. Every call shells to the CLI, which
// evaluates policy per request server-side, so a settings change applies to
// the next call without reloading Pi. Registration sits behind the same
// environment gate as the lifecycle handlers, and each execution re-checks
// SCRIBE_SESSION_ID so a registered tool no-ops outside a Scribe pane.

// 5-minute prompt ceiling + 1-minute action ceiling + 15-second CLI margin,
// plus 15 seconds for this child-process backstop to remain strictly outer.
const AGENT_CLI_TIMEOUT_MS = 390_000;

const AGENT_ACTION_NAMES = [
  "open-settings",
  "open-find",
  "new-tab",
  "new-claude-tab",
  "resume-claude-tab",
  "new-codex-tab",
  "resume-codex-tab",
  "split-vertical",
  "split-horizontal",
  "close-pane",
  "close-tab",
  "new-window",
  "switch-profile",
  "open-update-dialog",
  "focus-session",
];

type AgentToolParams = Record<string, string | number | boolean | undefined>;

type AgentToolSpec = {
  name: string;
  label: string;
  description: string;
  parameters: Record<string, unknown>;
  argv: (params: AgentToolParams) => string[];
};

function agentParameters(
  properties: Record<string, Record<string, unknown>>,
  required: string[] = [],
): Record<string, unknown> {
  return { type: "object", properties, required };
}

const AGENT_TOOLS: AgentToolSpec[] = [
  {
    name: "scribe_agent_world",
    label: "Scribe world",
    description:
      "List every window, workspace, and terminal session in the hosting Scribe terminal, including ids, AI state, and which session is this agent's own pane. Returns versioned JSON.",
    parameters: agentParameters({}),
    argv: () => ["world"],
  },
  {
    name: "scribe_agent_siblings",
    label: "Scribe siblings",
    description:
      "List the terminal sessions sharing this pane's Scribe window — the fastest way to find the pane next to this one. Returns versioned JSON.",
    parameters: agentParameters({}),
    argv: () => ["siblings"],
  },
  {
    name: "scribe_agent_read",
    label: "Scribe read pane",
    description:
      "Read a Scribe terminal session's visible screen text, optionally with trailing scrollback lines. Returns versioned JSON.",
    parameters: agentParameters(
      {
        session_id: {
          type: "string",
          description: "Full session id from scribe_agent_world or scribe_agent_siblings.",
        },
        scrollback: {
          type: "integer",
          description: "Scrollback lines to include above the visible screen.",
        },
      },
      ["session_id"],
    ),
    argv: (params) => [
      "read",
      String(params.session_id),
      ...(params.scrollback === undefined ? [] : ["--scrollback", String(params.scrollback)]),
    ],
  },
  {
    name: "scribe_agent_action",
    label: "Scribe action",
    description:
      "Dispatch a window action in the hosting Scribe terminal. switch-profile takes name; focus-session takes session_id; close-pane, close-tab, and open-update-dialog are destructive and gated separately. Returns versioned JSON.",
    parameters: agentParameters(
      {
        action: {
          type: "string",
          enum: AGENT_ACTION_NAMES,
          description: "Action to dispatch.",
        },
        name: { type: "string", description: "Profile name; only for switch-profile." },
        session_id: { type: "string", description: "Session to focus; only for focus-session." },
        window: {
          type: "string",
          description: "Target window id; defaults to this pane's window.",
        },
      },
      ["action"],
    ),
    argv: (params) => [
      "action",
      String(params.action),
      ...(params.name === undefined ? [] : [String(params.name)]),
      ...(params.session_id === undefined ? [] : [String(params.session_id)]),
      ...(params.window === undefined ? [] : ["--window", String(params.window)]),
    ],
  },
  {
    name: "scribe_agent_write",
    label: "Scribe write input",
    description:
      "Write text into a Scribe terminal session's input, optionally submitting it as a command. Returns versioned JSON.",
    parameters: agentParameters(
      {
        session_id: { type: "string", description: "Full session id of the target pane." },
        text: { type: "string", description: "Text to write into the pane's input." },
        submit: { type: "boolean", description: "Press Enter after writing the text." },
      },
      ["session_id", "text"],
    ),
    argv: (params) => [
      "write",
      String(params.session_id),
      "--text",
      String(params.text),
      ...(params.submit === true ? ["--submit"] : []),
    ],
  },
  {
    name: "scribe_agent_capabilities",
    label: "Scribe capabilities",
    description:
      "Show which Scribe agent API capabilities the user's policy allows, prompts for, or denies right now. Returns versioned JSON.",
    parameters: agentParameters({}),
    argv: () => ["capabilities"],
  },
];

/**
 * Run `scribe agent <argv> --agent pi` and resolve with the text the model
 * should see. Never rejects: a missing CLI, a failure envelope, and a killed
 * child all resolve to explanatory text, because the CLI's JSON envelope is
 * the contract and a thrown error would hide it.
 */
function runAgentCli(argv: string[], signal: AbortSignal | undefined): Promise<string> {
  return new Promise((resolve) => {
    const stdoutChunks: Buffer[] = [];
    const stderrChunks: Buffer[] = [];
    let settled = false;
    const finish = (text: string) => {
      if (settled) return;
      settled = true;
      resolve(text);
    };

    try {
      const child = spawn("scribe", ["agent", ...argv, "--agent", "pi"], {
        stdio: ["ignore", "pipe", "pipe"],
        windowsHide: true,
        signal,
        timeout: AGENT_CLI_TIMEOUT_MS,
      });
      child.stdout.on("data", (chunk: Buffer) => stdoutChunks.push(chunk));
      child.stderr.on("data", (chunk: Buffer) => stderrChunks.push(chunk));
      child.once("error", (error: Error) => {
        finish(`failed to run the scribe CLI: ${error.message}`);
      });
      child.once("close", (code) => {
        const stdout = Buffer.concat(stdoutChunks).toString("utf8").trim();
        const stderr = Buffer.concat(stderrChunks).toString("utf8").trim();
        const fallback =
          code === null ? "scribe agent was terminated" : `scribe agent exited with code ${code}`;
        finish(stdout || stderr || fallback);
      });
    } catch (error) {
      finish(
        `failed to run the scribe CLI: ${error instanceof Error ? error.message : String(error)}`,
      );
    }
  });
}

async function executeAgentCli(argv: string[], signal: AbortSignal | undefined) {
  if (!process.env.SCRIBE_SESSION_ID) {
    return {
      content: [
        {
          type: "text" as const,
          text: "SCRIBE_SESSION_ID is unset: this process is not inside a Scribe pane, so scribe agent tools are unavailable.",
        },
      ],
      details: undefined,
    };
  }
  const text = await runAgentCli(argv, signal);
  return { content: [{ type: "text" as const, text }], details: undefined };
}

// ── Tab mirror stream ───────────────────────────────────────────────────
//
// A Scribe tab mirror of this session can show more than the tab: Pi renders
// its fullscreen UI a second time at the mirror's size and streams those
// frames to Scribe through `scribe-hook-helper --event=mirror_stream`. The
// second render reuses pi-tui's own compositor (`TuiAltScreen.doRender`)
// against a terminal facade that reports the mirror's size, with the per-size
// render state swapped around the call, so the tab's own frame is untouched.
// Everything here rides on pi-tui internals and is feature-detected: when a
// name is missing the patch is not installed and the mirror falls back to
// Scribe's scaled copy.

type Any = Record<string, unknown>;
/** The pi-tui classes the mirror patches, loaded lazily so a pi-tui without
 *  one of them costs the mirror and nothing else in this extension. */
type TuiClasses = { Container: Any; Markdown: Any; Text: Any; TuiAltScreen: Any };

// Fields of TuiAltScreen/TuiBase that doRender reads or writes per frame.
const MIRROR_RENDER_STATE = [
  "previousScreen",
  "previousScreenWidth",
  "previousScreenHeight",
  "currentLayout",
  "lastDocument",
  "renderedOverlayLayouts",
  "scrollToEndIndicatorRect",
  "fullRedrawCount",
  "imageProtocol",
] as const;
// Fields of the primary ScrollView that layout mutates.
const MIRROR_SCROLL_STATE = [
  "currentScrollTop",
  "contentHeight",
  "currentViewportHeight",
  "followingEnd",
  "followSuppressedAtEnd",
] as const;
const MIRROR_HELPER_RESPAWNS = 3;
const MIRROR_HELPER_RESPAWN_MS = 5000;

function freshMirrorRenderState(): Any {
  return {
    previousScreen: [],
    previousScreenWidth: 0,
    previousScreenHeight: 0,
    currentLayout: undefined,
    lastDocument: [],
    renderedOverlayLayouts: [],
    scrollToEndIndicatorRect: undefined,
    fullRedrawCount: 0,
    // Images would be uploaded to the tab's terminal; the mirror shows their
    // text fallback instead.
    imageProtocol: null,
  };
}

// Text, Markdown, and pi-coding-agent's VisualLinePreview remember one
// rendered width; two viewports would evict each other every frame and
// re-render the whole transcript twice. Keep the last two widths beside the
// component and treat its own cache being cleared as the invalidation signal.
function multiWidthCache(proto: Any): () => void {
  const original = proto.render as (this: Any, width: number) => string[];
  const side = new WeakMap<object, Map<number, { text: unknown; lines: string[] }>>();
  proto.render = function cached(this: Any, width: number) {
    let slots = side.get(this);
    if (this.cachedLines === undefined) {
      slots?.clear();
    } else if (slots) {
      const hit = slots.get(width);
      if (hit && hit.text === this.text) return hit.lines;
    }
    const lines = original.call(this, width);
    if (!slots) {
      slots = new Map();
      side.set(this, slots);
    }
    slots.set(width, { text: this.text, lines });
    if (slots.size > 2) slots.delete(slots.keys().next().value as number);
    return lines;
  };
  return () => {
    proto.render = original;
  };
}

// Give every child class with a one-width `cachedLines` cache the side cache
// the first time one of its instances renders under a Container. Mirrors
// `Container.render`, including the mouse layout it records.
function hookContainerRender(
  proto: Any,
  patched: WeakSet<object>,
  restores: (() => void)[],
): () => void {
  const original = proto.render as (this: Any, width: number) => string[];
  proto.render = function hooked(this: Any, width: number) {
    const lines: string[] = [];
    const mouseChildren: { component: Any; height: number }[] = [];
    for (const child of this.children as Any[]) {
      if ("cachedLines" in child && "cachedWidth" in child) {
        const childProto = Object.getPrototypeOf(child) as object | null;
        if (childProto && !patched.has(childProto)) {
          patched.add(childProto);
          restores.push(multiWidthCache(childProto as Any));
        }
      }
      const childLines = (child.render as (width: number) => string[]).call(child, width);
      mouseChildren.push({ component: child, height: childLines.length });
      for (const line of childLines) lines.push(line);
    }
    this.mouseLayout = { width, children: mouseChildren };
    return lines;
  };
  return () => {
    proto.render = original;
  };
}

async function loadTuiClasses(): Promise<TuiClasses | undefined> {
  let module: Partial<TuiClasses>;
  try {
    module = (await import("@earendil-works/pi-tui")) as Partial<TuiClasses>;
  } catch {
    return undefined;
  }
  const { Container, Markdown, Text, TuiAltScreen } = module;
  const proto = (cls: unknown) => (cls as { prototype?: Any } | undefined)?.prototype;
  const alt = proto(TuiAltScreen);
  const present =
    typeof alt?.doRender === "function" &&
    typeof alt.getPrimaryScrollView === "function" &&
    typeof proto(Container)?.render === "function" &&
    typeof proto(Text)?.render === "function" &&
    typeof proto(Markdown)?.render === "function";
  if (!present || !Container || !Markdown || !Text || !TuiAltScreen) return undefined;
  return { Container, Markdown, Text, TuiAltScreen };
}

/** Whether a live TUI and its scroll view carry every field the swap needs. */
function mirrorInstanceSupported(tui: Any, scroll: Any): boolean {
  return (
    "terminal" in tui &&
    "altScreenActive" in tui &&
    "stopped" in tui &&
    MIRROR_RENDER_STATE.every((key) => key in tui) &&
    MIRROR_SCROLL_STATE.every((key) => key in scroll) &&
    typeof scroll.scrollBy === "function"
  );
}

class MirrorStream {
  private viewport: { cols: number; rows: number } | undefined;
  private renderState = freshMirrorRenderState();
  // The mirror's own scroll position, `undefined` until its first frame, which
  // follows the transcript's end.
  private scrollState: Any | undefined;
  private pendingScroll = 0;
  private child: ChildProcess | undefined;
  private respawns = 0;
  private respawnTimer: ReturnType<typeof setTimeout> | undefined;
  private stopped = false;
  private disabled = false;
  private restores: (() => void)[] = [];
  // The live TUI, for a render when the mirror opens or scrolls; the one the
  // host hands widget factories, which follows a regular/fullscreen switch.
  tui: Any | undefined;

  constructor(private readonly helperPath: string) {}

  private starting = false;

  async start() {
    if (this.stopped || this.disabled || this.starting || this.restores.length) return;
    this.starting = true;
    const classes = await loadTuiClasses();
    this.starting = false;
    if (this.stopped) return;
    if (!classes) {
      this.disabled = true;
      return;
    }
    const patched = new WeakSet<object>();
    for (const cls of [classes.Text, classes.Markdown]) {
      const proto = cls.prototype as Any;
      patched.add(proto);
      this.restores.push(multiWidthCache(proto));
    }
    const container = classes.Container.prototype as Any;
    this.restores.push(hookContainerRender(container, patched, this.restores));
    const alt = classes.TuiAltScreen.prototype as Any;
    const original = alt.doRender as (this: Any) => void;
    const stream = this;
    alt.doRender = function patchedDoRender(this: Any) {
      stream.render(this, original);
    };
    this.restores.push(() => {
      alt.doRender = original;
    });
    this.spawnHelper();
  }

  stop() {
    this.stopped = true;
    if (this.respawnTimer) clearTimeout(this.respawnTimer);
    this.respawnTimer = undefined;
    for (const restore of this.restores.splice(0).reverse()) restore();
    const child = this.child;
    this.child = undefined;
    if (child) {
      try {
        child.stdin?.end();
        child.kill("SIGTERM");
      } catch {
        // Best effort only.
      }
    }
  }

  private spawnHelper() {
    if (this.stopped || this.disabled) return;
    let child: ChildProcess;
    try {
      child = spawn(this.helperPath, ["--provider=pi", "--event=mirror_stream"], {
        stdio: ["pipe", "pipe", "ignore"],
        windowsHide: true,
      });
    } catch {
      return;
    }
    this.child = child;
    child.stdin?.on("error", () => {});
    let buffered = "";
    child.stdout?.setEncoding("utf8");
    child.stdout?.on("data", (chunk: string) => {
      buffered += chunk;
      let newline = buffered.indexOf("\n");
      while (newline !== -1) {
        this.onCommand(buffered.slice(0, newline));
        buffered = buffered.slice(newline + 1);
        newline = buffered.indexOf("\n");
      }
    });
    child.once("error", () => {});
    child.once("close", () => {
      if (this.child !== child) return;
      this.child = undefined;
      this.setViewport(undefined);
      // A server without mirror streams closes at once; three tries bound
      // the cost of asking an older server forever.
      if (this.stopped || this.respawns >= MIRROR_HELPER_RESPAWNS) return;
      this.respawns += 1;
      this.respawnTimer = setTimeout(() => {
        this.respawnTimer = undefined;
        this.spawnHelper();
      }, MIRROR_HELPER_RESPAWN_MS);
    });
  }

  private onCommand(line: string) {
    let command: { cols?: unknown; rows?: unknown; scroll?: unknown };
    try {
      command = JSON.parse(line) as typeof command;
    } catch {
      return;
    }
    if (typeof command.scroll === "number" && Number.isFinite(command.scroll)) {
      // Scribe counts positive as back into history; ScrollView counts
      // positive as toward the end.
      this.pendingScroll -= Math.trunc(command.scroll);
      this.requestRender();
      return;
    }
    if (typeof command.cols !== "number" || typeof command.rows !== "number") return;
    const cols = Math.trunc(command.cols);
    const rows = Math.trunc(command.rows);
    this.setViewport(cols > 0 && rows > 0 ? { cols, rows } : undefined);
  }

  private setViewport(viewport: { cols: number; rows: number } | undefined) {
    this.viewport = viewport;
    if (!viewport) {
      // The next mirror starts from a full redraw at the transcript's end.
      this.renderState = freshMirrorRenderState();
      this.scrollState = undefined;
      this.pendingScroll = 0;
      return;
    }
    this.requestRender();
  }

  private requestRender() {
    try {
      (this.tui?.requestRender as (() => void) | undefined)?.call(this.tui);
    } catch {
      // The TUI may be stopped; the next natural render still carries the frame.
    }
  }

  /** Render the mirror's frame, then the tab's, in that order so every piece
   *  of state the tab depends on (editor wrap width, overlay hit-test bounds,
   *  scroll geometry) is left as the tab's own render set it. */
  private render(tui: Any, original: (this: Any) => void) {
    const viewport = this.viewport;
    if (!viewport || this.disabled || tui.stopped || !tui.altScreenActive) {
      original.call(tui);
      return;
    }
    this.tui ??= tui;
    const scroll = (tui.getPrimaryScrollView as () => Any).call(tui);
    if (!mirrorInstanceSupported(tui, scroll)) {
      this.disabled = true;
      original.call(tui);
      return;
    }
    const savedTui: Any = {};
    for (const key of MIRROR_RENDER_STATE) savedTui[key] = tui[key];
    const savedTerminal = tui.terminal as object;
    const savedScroll: Any = {};
    for (const key of MIRROR_SCROLL_STATE) savedScroll[key] = scroll[key];
    let out = "";
    const facade = Object.create(savedTerminal) as Any;
    Object.defineProperties(facade, {
      columns: { get: () => viewport.cols },
      rows: { get: () => viewport.rows },
      write: {
        value: (data: string) => {
          out += data;
        },
      },
    });
    try {
      for (const key of MIRROR_RENDER_STATE) tui[key] = this.renderState[key];
      tui.terminal = facade;
      if (this.scrollState) {
        Object.assign(scroll, this.scrollState);
        if (this.pendingScroll !== 0) {
          (scroll.scrollBy as (lines: number) => number).call(scroll, this.pendingScroll);
        }
      } else {
        scroll.followingEnd = true;
      }
      this.pendingScroll = 0;
      original.call(tui);
      for (const key of MIRROR_RENDER_STATE) this.renderState[key] = tui[key];
      const next: Any = {};
      for (const key of MIRROR_SCROLL_STATE) next[key] = scroll[key];
      this.scrollState = next;
    } catch {
      this.disabled = true;
    } finally {
      for (const key of MIRROR_RENDER_STATE) tui[key] = savedTui[key];
      tui.terminal = savedTerminal;
      Object.assign(scroll, savedScroll);
    }
    if (out && this.child?.stdin?.writable) {
      const frame = JSON.stringify({ cols: viewport.cols, rows: viewport.rows, frame: out });
      this.child.stdin.write(`${frame}\n`);
    }
    original.call(tui);
  }
}

function registerAgentTools(pi: ExtensionAPI) {
  // Older Pi runtimes without registerTool keep lifecycle reporting only.
  if (typeof pi.registerTool !== "function") return;
  for (const tool of AGENT_TOOLS) {
    pi.registerTool({
      name: tool.name,
      label: tool.label,
      description: tool.description,
      parameters: tool.parameters,
      execute: (_toolCallId: string, params: unknown, signal: AbortSignal | undefined) =>
        executeAgentCli(tool.argv((params ?? {}) as AgentToolParams), signal),
    } as Parameters<ExtensionAPI["registerTool"]>[0]);
  }
}

// @lat: [[server#Server#Hook Channel#Pi Extension Adapter]]
export default function scribePiExtension(pi: ExtensionAPI) {
  const helper = process.env.SCRIBE_HOOK_HELPER;
  if (
    !helper ||
    !process.env.SCRIBE_HOOK_SOCK ||
    !process.env.SCRIBE_SESSION_ID ||
    process.env.PI_SUBAGENT_CHILD === "1"
  ) {
    return;
  }

  const helperPath = helper;
  const globalRegistry = globalThis as unknown as Record<PropertyKey, unknown>;
  if (globalRegistry[REGISTRATION]) return;
  const registration = {};
  globalRegistry[REGISTRATION] = registration;

  let generation = 0;
  let active: Promise<void> | undefined;
  let pending: QueuedEvent[] = [];
  let capturedInputs = 0;
  let latestAssistant = "";
  let latestError = false;
  let shuttingDown = false;
  let shutdownPromise: Promise<void> | undefined;
  let parentState: { event: "state_changed" | "session_stopped"; payload: Payload } = {
    event: "state_changed", payload: { state: "idle_prompt" },
  };
  let askingUser = false;
  let subagentsRunning = false;
  // Running pi-background-tasks work: the tasks that will wake this agent,
  // whether one of them has finished but the turn it wakes has not started
  // yet, and whether any running task will not wake it.
  let wakingTasks = new Set<string>();
  let backgroundWake = false;
  let manualTasks = false;
  let backgroundRequestId: string | undefined;
  let backgroundRefreshQueued = false;
  let lastPercent: number | undefined;
  let refreshQueued = false;
  let unsubscribeSubagentStatus: (() => void) | undefined;
  const mirror = new MirrorStream(helperPath);

  // What running background work leaves a settled agent waiting on: its own
  // wake-up when a task will start another turn, otherwise the user, because
  // a task that will not wake it leaves only the user able to resume it.
  function backgroundWait() {
    if (wakingTasks.size > 0 || backgroundWake) return "waiting_for_background";
    return manualTasks ? "waiting_for_input" : undefined;
  }

  function reportState() {
    const wait = parentState.payload.state === "processing" ? undefined : backgroundWait();
    if (askingUser) enqueue("state_changed", { state: "waiting_for_input" });
    else if (subagentsRunning) enqueue("state_changed", { state: "processing" });
    else if (wait) enqueue("state_changed", { state: wait });
    else enqueue(parentState.event, parentState.payload);
  }

  function setParentState(event: typeof parentState.event, payload: Payload) {
    parentState = { event, payload };
    reportState();
  }

  // Context is read at every turn boundary, not only at settle: a long run
  // otherwise pins the gauge at its pre-run value, and an auto-compaction
  // inside that run (which drops usage by ~90%) stays invisible until the run
  // ends. Right after compaction Pi reports `percent: null` until the next
  // assistant response, so the compaction itself emits nothing and the next
  // `turn_end` carries the post-compaction reading.
  function reportContext(ctx: Parameters<typeof contextPercent>[0]) {
    const percent = contextPercent(ctx);
    if (percent === undefined || percent === lastPercent) return;
    lastPercent = percent;
    enqueue("context_changed", { fill_percent: percent });
  }

  // Ask the optional pi-subagents owner for its current-session projection.
  // Events only invalidate it: counting start/complete edges loses restored
  // workflows and detached foreground work.
  function refreshSubagents() {
    unsubscribeSubagentStatus?.();
    unsubscribeSubagentStatus = undefined;
    if (shuttingDown || refreshQueued) return;
    refreshQueued = true;
    queueMicrotask(() => {
      refreshQueued = false;
      if (shuttingDown) return;
      const requestId = randomUUID();
      unsubscribeSubagentStatus = pi.events.on(`subagents:rpc:v1:reply:${requestId}`, (payload) => {
        unsubscribeSubagentStatus?.();
        unsubscribeSubagentStatus = undefined;
        const reply = payload as {
          version?: unknown;
          requestId?: unknown;
          success?: unknown;
          data?: { fleet?: { version?: unknown; totalActive?: unknown } };
        } | undefined;
        const fleet = reply?.data?.fleet;
        if (
          reply?.version !== 1 || reply.requestId !== requestId || reply.success !== true ||
          fleet?.version !== 1 || typeof fleet.totalActive !== "number" ||
          !Number.isSafeInteger(fleet.totalActive) || fleet.totalActive < 0
        ) return;
        const running = fleet.totalActive > 0;
        if (subagentsRunning === running) return;
        subagentsRunning = running;
        reportState();
      });
      pi.events.emit("subagents:rpc:v1:request", { version: 1, requestId, method: "status" });
    });
  }

  const unsubscribeSubagents = [
    "subagents:rpc:v1:ready",
    "subagent:async-started",
    "subagent:async-complete",
    "subagent:foreground-complete",
    "subagent:process-terminal",
    "subagent:child-status",
  ].map((event) => pi.events.on(event, refreshSubagents));

  // Ask the optional pi-background-tasks owner which of its tasks still run.
  // Only tasks that notify and trigger a turn count: the agent resumes when
  // they finish, so the pane waits on them rather than on the user. The newest
  // request supersedes any reply still in flight.
  function refreshBackgroundTasks() {
    if (shuttingDown || backgroundRefreshQueued) return;
    backgroundRefreshQueued = true;
    queueMicrotask(() => {
      backgroundRefreshQueued = false;
      if (shuttingDown) return;
      backgroundRequestId = randomUUID();
      pi.events.emit(BG_REQUEST_CHANNEL, {
        schema_version: "pi-background-tasks.extension-request.v1",
        request_id: backgroundRequestId,
        operation: "status",
        payload: {},
      });
    });
  }

  const unsubscribeBackgroundStatus = pi.events.on(BG_RESPONSE_CHANNEL, (payload) => {
    const response = payload as
      | { request_id?: unknown; ok?: unknown; result?: { tasks?: unknown } }
      | undefined;
    if (!response || !backgroundRequestId || response.request_id !== backgroundRequestId) return;
    backgroundRequestId = undefined;
    const tasks = response.ok === true ? response.result?.tasks : undefined;
    if (!Array.isArray(tasks)) return;
    const before = backgroundWait();
    const running = tasks.filter(runningTask);
    wakingTasks = new Set(running.filter(wakesAgent).map((task) => task.id));
    manualTasks = running.some((task) => !wakesAgent(task));
    if (before !== backgroundWait()) reportState();
  });

  // A tracked task's completion notification is about to wake the agent, so
  // keep waiting until that turn starts instead of flashing the settled state
  // (and its desktop notification) in between. Once the refresh drops the id,
  // a repeated frame for the same task changes nothing.
  const unsubscribeBackgroundTerminal = pi.events.on(BG_TERMINAL_CHANNEL, (payload) => {
    const id = (payload as { task?: { id?: unknown } } | undefined)?.task?.id;
    if (typeof id === "string" && wakingTasks.has(id)) backgroundWake = true;
    refreshBackgroundTasks();
  });

  const unsubscribeAskUserBlocked = pi.events.on(ASK_USER_BLOCKED_EVENT, (payload) => {
    if (!payload || typeof payload !== "object" || typeof (payload as { active?: unknown }).active !== "boolean") return;
    askingUser = (payload as { active: boolean }).active;
    reportState();
  });

  function invoke(event: EventName, payload: Payload): Promise<void> {
    return new Promise((resolve) => {
      let settled = false;
      let timer: ReturnType<typeof setTimeout> | undefined;
      const finish = () => {
        if (settled) return;
        settled = true;
        if (timer) clearTimeout(timer);
        resolve();
      };

      try {
        const child = spawn(
          helperPath,
          ["--provider=pi", `--event=${event}`, "--payload-stdin"],
          { stdio: ["pipe", "ignore", "ignore"], windowsHide: true },
        );
        timer = setTimeout(() => {
          try {
            child.kill("SIGKILL");
          } catch {
            // Best effort only.
          }
          finish();
        }, HELPER_TIMEOUT_MS);

        child.once("error", finish);
        child.once("close", finish);
        child.stdin.on("error", () => {});
        child.stdin.end(JSON.stringify(payload));
      } catch {
        finish();
      }
    });
  }

  function drain() {
    if (active) return;
    while (pending[0] && pending[0].generation !== generation) pending.shift();
    const next = pending.shift();
    if (!next) return;
    active = invoke(next.event, next.payload).finally(() => {
      active = undefined;
      drain();
    });
  }

  function enqueue(event: EventName, payload: Payload = {}) {
    if (shuttingDown || pending.length + (active ? 1 : 0) >= MAX_OUTSTANDING) return;
    pending.push({ event, payload, generation });
    drain();
  }

  pi.on("session_start", (_event, ctx) => {
    capturedInputs = 0;
    latestAssistant = "";
    latestError = false;
    lastPercent = undefined;
    if (ctx.mode === "tui") {
      // A zero-line widget is the one public path to the live TUI handle;
      // it is removed again in the same tick, so the layout never changes.
      try {
        ctx.ui.setWidget("scribe-mirror-probe", (tui) => {
          mirror.tui = tui as unknown as Any;
          return { render: () => [], invalidate: () => {} };
        });
        ctx.ui.setWidget("scribe-mirror-probe", undefined);
      } catch {
        // Without the handle the mirror still renders on Pi's next natural frame.
      }
      void mirror.start();
    }
    enqueue("task_label_cleared");
    setParentState("state_changed", { state: "idle_prompt" });
    refreshSubagents();
    refreshBackgroundTasks();
  });

  pi.on("input", (event) => {
    if (event.source === "extension") return { action: "continue" };
    capturedInputs += 1;
    latestAssistant = "";
    latestError = false;
    setParentState("state_changed", { state: "processing" });
    if (typeof event.text === "string" && event.text) {
      enqueue("prompt_received", { text: event.text });
      const label = taskLabel(event.text);
      if (label) enqueue("task_label_changed", { label });
    }
    return { action: "continue" };
  });

  pi.on("agent_start", () => {
    backgroundWake = false;
    if (capturedInputs > 0) capturedInputs -= 1;
    else setParentState("state_changed", { state: "processing" });
  });

  pi.on("tool_result", (event) => {
    if (event.toolName === "subagent") refreshSubagents();
  });

  // A turn's tool results may have launched background work, and a completion
  // delivered mid-run is consumed by the next turn rather than waking a new one.
  pi.on("turn_end", (_event, ctx) => {
    backgroundWake = false;
    refreshBackgroundTasks();
    reportContext(ctx);
  });

  pi.on("message_end", (event) => {
    const text = assistantText(event.message);
    if (text === undefined) return;
    latestAssistant = text;
    latestError =
      !!event.message &&
      typeof event.message === "object" &&
      (event.message as { stopReason?: unknown }).stopReason === "error";
  });

  pi.on("agent_settled", (_event, ctx) => {
    capturedInputs = 0;
    if (latestError) setParentState("state_changed", { state: "error" });
    else setParentState("session_stopped", { last_message: latestAssistant });
    refreshSubagents();
    refreshBackgroundTasks();
    reportContext(ctx);
  });

  // The exact issue-to-session join behind Flow's live-agent halo: the id
  // comes from the observed claim and the session from SCRIBE_SESSION_ID in
  // this process's own environment, so nothing has to guess whether a
  // generated assignee string names the agent running in this pane.
  //
  // Observation only — the handler never blocks and never mutates
  // `event.input`, so a claim that Scribe cannot report still runs normally.
  pi.on("tool_call", (event) => {
    if (event.toolName !== "bash") return;
    const command = (event.input as { command?: unknown })?.command;
    if (typeof command !== "string" || !command) return;
    const issueId = claimedIssueId(command);
    if (issueId) enqueue("issue_focused", { issue_id: issueId });
  });

  pi.on("session_shutdown", () => {
    shutdownPromise ??= (async () => {
      shuttingDown = true;
      unsubscribeAskUserBlocked();
      for (const unsubscribe of unsubscribeSubagents) unsubscribe();
      unsubscribeSubagentStatus?.();
      unsubscribeBackgroundStatus();
      unsubscribeBackgroundTerminal();
      generation += 1;
      pending = [];
      mirror.stop();
      if (active) await active;
      await invoke("state_cleared", {});
      if (globalRegistry[REGISTRATION] === registration) delete globalRegistry[REGISTRATION];
    })();
    return shutdownPromise;
  });

  registerAgentTools(pi);
}

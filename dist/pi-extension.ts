// SCRIBE-MANAGED-PI-EXTENSION
// Scribe lifecycle adapter for Pi. Installed at user scope by Scribe.

import { spawn } from "node:child_process";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

const REGISTRATION = Symbol.for("scribe.pi.lifecycle-extension");
const MAX_OUTSTANDING = 32;
const HELPER_TIMEOUT_MS = 100;
const TASK_LABEL_LIMIT = 120;

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

const AGENT_CLI_TIMEOUT_MS = 10_000;

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

  pi.on("session_start", () => {
    capturedInputs = 0;
    latestAssistant = "";
    latestError = false;
    enqueue("task_label_cleared");
    enqueue("state_changed", { state: "idle_prompt" });
  });

  pi.on("input", (event) => {
    if (event.source === "extension") return { action: "continue" };
    capturedInputs += 1;
    latestAssistant = "";
    latestError = false;
    enqueue("state_changed", { state: "processing" });
    if (typeof event.text === "string" && event.text) {
      enqueue("prompt_received", { text: event.text });
      const label = taskLabel(event.text);
      if (label) enqueue("task_label_changed", { label });
    }
    return { action: "continue" };
  });

  pi.on("agent_start", () => {
    if (capturedInputs > 0) capturedInputs -= 1;
    else enqueue("state_changed", { state: "processing" });
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
    if (latestError) enqueue("state_changed", { state: "error" });
    else enqueue("session_stopped", { last_message: latestAssistant });
    const percent = contextPercent(ctx);
    if (percent !== undefined) enqueue("context_changed", { fill_percent: percent });
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
      generation += 1;
      pending = [];
      if (active) await active;
      await invoke("state_cleared", {});
      if (globalRegistry[REGISTRATION] === registration) delete globalRegistry[REGISTRATION];
    })();
    return shutdownPromise;
  });

  registerAgentTools(pi);
}

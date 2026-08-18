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
  | "context_changed";

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
}

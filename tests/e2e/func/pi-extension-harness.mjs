#!/usr/bin/env node

import assert from "node:assert/strict";
import { chmod, mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { randomUUID } from "node:crypto";

const here = dirname(fileURLToPath(import.meta.url));
const extensionPath = resolve(here, "../../../dist/pi-extension.ts");
const ASK_USER_BLOCKED_EVENT = "rpiv:ask-user:blocked";
const tempDir = await mkdtemp(join(tmpdir(), "scribe-pi-extension-"));
const helperPath = join(tempDir, "fake-helper.mjs");

const helperSource = `#!/usr/bin/env node
import { appendFileSync } from "node:fs";
const chunks = [];
for await (const chunk of process.stdin) chunks.push(chunk);
const record = {
  phase: "start",
  argv: process.argv.slice(2),
  stdin: Buffer.concat(chunks).toString("utf8"),
  time: Date.now(),
  pid: process.pid,
};
appendFileSync(process.env.FAKE_HELPER_LOG, JSON.stringify(record) + "\\n");
const sleepMs = Number(process.env.FAKE_HELPER_SLEEP_MS || 0);
if (sleepMs > 0) await new Promise((resolve) => setTimeout(resolve, sleepMs));
appendFileSync(process.env.FAKE_HELPER_LOG, JSON.stringify({
  phase: "end",
  time: Date.now(),
  pid: process.pid,
}) + "\\n");
`;

await writeFile(helperPath, helperSource);
await chmod(helperPath, 0o755);

const originalEnv = {
  SCRIBE_HOOK_HELPER: process.env.SCRIBE_HOOK_HELPER,
  SCRIBE_HOOK_SOCK: process.env.SCRIBE_HOOK_SOCK,
  SCRIBE_SESSION_ID: process.env.SCRIBE_SESSION_ID,
  PI_SUBAGENT_CHILD: process.env.PI_SUBAGENT_CHILD,
  FAKE_HELPER_LOG: process.env.FAKE_HELPER_LOG,
  FAKE_HELPER_SLEEP_MS: process.env.FAKE_HELPER_SLEEP_MS,
};

let importCounter = 0;
const { default: extensionFactory } = await import(
  `${pathToFileURL(extensionPath).href}?harness=${importCounter++}`
);

class FakeEventBus {
  handlers = new Map();

  on(name, handler) {
    const handlers = this.handlers.get(name) ?? new Set();
    handlers.add(handler);
    this.handlers.set(name, handlers);
    return () => handlers.delete(handler);
  }

  emit(name, payload) {
    for (const handler of this.handlers.get(name) ?? []) handler(payload);
  }
}

class FakeExtensionAPI {
  handlers = new Map();
  tools = new Map();
  events = new FakeEventBus();

  on(name, handler) {
    const handlers = this.handlers.get(name) ?? [];
    handlers.push(handler);
    this.handlers.set(name, handlers);
  }

  registerTool(tool) {
    assert.ok(!this.tools.has(tool.name), `duplicate tool registration: ${tool.name}`);
    this.tools.set(tool.name, tool);
  }

  handler(name) {
    const handlers = this.handlers.get(name) ?? [];
    assert.equal(handlers.length, 1, `expected one ${name} handler`);
    return handlers[0];
  }

  count(name) {
    return (this.handlers.get(name) ?? []).length;
  }
}

function makeContext(percent = null) {
  return {
    getContextUsage() {
      return percent === undefined ? undefined : { tokens: 1, contextWindow: 2, percent };
    },
  };
}

function setHarnessEnv(logPath, sleepMs = 0, helper = helperPath) {
  process.env.SCRIBE_HOOK_HELPER = helper;
  process.env.SCRIBE_HOOK_SOCK = join(tempDir, "scribe.sock");
  process.env.SCRIBE_SESSION_ID = randomUUID();
  delete process.env.PI_SUBAGENT_CHILD;
  process.env.FAKE_HELPER_LOG = logPath;
  process.env.FAKE_HELPER_SLEEP_MS = String(sleepMs);
}

function restoreEnv() {
  for (const [name, value] of Object.entries(originalEnv)) {
    if (value === undefined) delete process.env[name];
    else process.env[name] = value;
  }
}

async function records(logPath) {
  let text;
  try {
    text = await readFile(logPath, "utf8");
  } catch (error) {
    if (error?.code === "ENOENT") return [];
    throw error;
  }
  return text.trim() ? text.trim().split("\n").map((line) => JSON.parse(line)) : [];
}

async function starts(logPath) {
  return (await records(logPath)).filter((record) => record.phase === "start");
}

async function waitFor(test, message, timeoutMs = 5000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const value = await test();
    if (value) return value;
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
  assert.fail(message);
}

function parsedCalls(entries) {
  return entries.map((entry) => ({
    event: entry.argv[1]?.replace("--event=", ""),
    payload: JSON.parse(entry.stdin),
  }));
}

function assertFixedArgv(entries) {
  const allowed = new Set([
    "state_changed",
    "session_stopped",
    "state_cleared",
    "prompt_received",
    "task_label_changed",
    "task_label_cleared",
    "context_changed",
    "issue_focused",
  ]);
  for (const entry of entries) {
    assert.equal(entry.argv.length, 3, "helper argv must contain fixed selectors only");
    assert.equal(entry.argv[0], "--provider=pi");
    assert.match(entry.argv[1], /^--event=[a-z_]+$/);
    assert.ok(allowed.has(entry.argv[1].slice("--event=".length)));
    assert.equal(entry.argv[2], "--payload-stdin");
    assert.doesNotThrow(() => JSON.parse(entry.stdin));
  }
}

function assertNoPermissionPrompt(entries) {
  for (const { payload } of parsedCalls(entries)) {
    assert.notEqual(payload.state, "permission_prompt");
  }
}

async function shutdown(api, reason = "quit") {
  if (api.count("session_shutdown") === 0) return;
  await api.handler("session_shutdown")({ type: "session_shutdown", reason }, makeContext());
}

// @lat: [[test#Test Harness#Pi Extension Harness#Startup and duplicate guard]]
async function testStartupAndDuplicateGuard() {
  const logPath = join(tempDir, "startup.jsonl");
  setHarnessEnv(logPath);
  const first = new FakeExtensionAPI();
  const duplicate = new FakeExtensionAPI();
  extensionFactory(first);
  extensionFactory(duplicate);
  assert.equal(duplicate.handlers.size, 0, "duplicate load must not register handlers");

  first.handler("session_start")({ type: "session_start", reason: "startup" }, makeContext());
  const entries = await waitFor(async () => {
    const value = await starts(logPath);
    return value.length >= 2 ? value : null;
  }, "startup events did not arrive");
  assert.deepEqual(parsedCalls(entries.slice(0, 2)), [
    { event: "task_label_cleared", payload: {} },
    { event: "state_changed", payload: { state: "idle_prompt" } },
  ]);
  await shutdown(first);
  return starts(logPath);
}

// @lat: [[test#Test Harness#Pi Extension Harness#Input sources and order]]
async function testInputSourcesAndOrder() {
  const logPath = join(tempDir, "input.jsonl");
  setHarnessEnv(logPath);
  const api = new FakeExtensionAPI();
  extensionFactory(api);
  const input = api.handler("input");
  const ctx = makeContext();

  const prompt = "\n /reload\n Build the thing; safely\u0001";
  assert.deepEqual(
    input({ type: "input", text: prompt, source: "interactive" }, ctx),
    { action: "continue" },
  );
  await waitFor(async () => (await starts(logPath)).length >= 3, "interactive events missing");
  assert.deepEqual(parsedCalls((await starts(logPath)).slice(0, 3)), [
    { event: "state_changed", payload: { state: "processing" } },
    { event: "prompt_received", payload: { text: prompt } },
    { event: "task_label_changed", payload: { label: "Build the thing, safely" } },
  ]);

  input({ type: "input", text: "RPC request", source: "rpc" }, ctx);
  await waitFor(async () => (await starts(logPath)).length >= 6, "RPC events missing");
  assert.deepEqual(parsedCalls((await starts(logPath)).slice(3, 6)), [
    { event: "state_changed", payload: { state: "processing" } },
    { event: "prompt_received", payload: { text: "RPC request" } },
    { event: "task_label_changed", payload: { label: "RPC request" } },
  ]);

  const before = (await starts(logPath)).length;
  input({ type: "input", text: "machine turn", source: "extension" }, ctx);
  await new Promise((resolve) => setTimeout(resolve, 30));
  assert.equal((await starts(logPath)).length, before, "extension input must be ignored");
  await shutdown(api);
  return starts(logPath);
}

// @lat: [[test#Test Harness#Pi Extension Harness#Shared questionnaire wait]]
async function testSharedQuestionnaireWait() {
  const logPath = join(tempDir, "questionnaire.jsonl");
  setHarnessEnv(logPath);
  const api = new FakeExtensionAPI();
  extensionFactory(api);
  const stateCalls = async () =>
    parsedCalls(await starts(logPath))
      .filter(({ event }) => event === "state_changed")
      .map(({ payload }) => payload.state);

  api.handler("input")({ type: "input", text: "Wait for a choice", source: "interactive" }, makeContext());
  await waitFor(async () => (await stateCalls()).length >= 1, "initial Processing state missing");
  assert.deepEqual(await stateCalls(), ["processing"]);

  // Opening blocks Pi inside the still-running tool.
  api.events.emit(ASK_USER_BLOCKED_EVENT, { active: true });
  await waitFor(async () => (await stateCalls()).length >= 2, "questionnaire open state missing");
  assert.deepEqual(await stateCalls(), ["processing", "waiting_for_input"]);

  // Answering unblocks it; reopening then cancelling does the same.
  api.events.emit(ASK_USER_BLOCKED_EVENT, { active: false });
  await waitFor(async () => (await stateCalls()).length >= 3, "questionnaire answer state missing");
  assert.deepEqual(await stateCalls(), ["processing", "waiting_for_input", "processing"]);

  api.events.emit(ASK_USER_BLOCKED_EVENT, { active: true });
  api.events.emit(ASK_USER_BLOCKED_EVENT, { active: false });
  await waitFor(async () => (await stateCalls()).length >= 5, "questionnaire cancel state missing");
  assert.deepEqual(await stateCalls(), [
    "processing",
    "waiting_for_input",
    "processing",
    "waiting_for_input",
    "processing",
  ]);

  for (const payload of [undefined, null, {}, { active: "true" }, { active: 1 }]) {
    api.events.emit(ASK_USER_BLOCKED_EVENT, payload);
  }
  await new Promise((resolve) => setTimeout(resolve, 30));
  assert.deepEqual(await stateCalls(), [
    "processing",
    "waiting_for_input",
    "processing",
    "waiting_for_input",
    "processing",
  ]);

  await shutdown(api);
  return starts(logPath);
}

// @lat: [[test#Test Harness#Pi Extension Harness#Retry and settle behavior]]
async function testRetryAndSettleBehavior() {
  const logPath = join(tempDir, "settle.jsonl");
  setHarnessEnv(logPath);
  const api = new FakeExtensionAPI();
  extensionFactory(api);
  const input = api.handler("input");
  const agentStart = api.handler("agent_start");
  const messageEnd = api.handler("message_end");
  const settled = api.handler("agent_settled");

  input({ type: "input", text: "Normal task", source: "interactive" }, makeContext());
  agentStart({ type: "agent_start" }, makeContext());
  agentStart({ type: "agent_start" }, makeContext());
  messageEnd({
    type: "message_end",
    message: {
      role: "assistant",
      content: [{ type: "thinking", thinking: "hidden" }, { type: "text", text: "Done." }],
      stopReason: "stop",
    },
  }, makeContext());
  settled({ type: "agent_settled" }, makeContext(49.6));
  await waitFor(async () => (await starts(logPath)).length >= 6, "normal settle events missing");
  assert.deepEqual(parsedCalls((await starts(logPath)).slice(0, 6)), [
    { event: "state_changed", payload: { state: "processing" } },
    { event: "prompt_received", payload: { text: "Normal task" } },
    { event: "task_label_changed", payload: { label: "Normal task" } },
    { event: "state_changed", payload: { state: "processing" } },
    { event: "session_stopped", payload: { last_message: "Done." } },
    { event: "context_changed", payload: { fill_percent: 50 } },
  ]);

  input({ type: "input", text: "Question task", source: "rpc" }, makeContext());
  agentStart({ type: "agent_start" }, makeContext());
  messageEnd({
    type: "message_end",
    message: { role: "assistant", content: [{ type: "text", text: "Which option should I use?" }], stopReason: "stop" },
  }, makeContext());
  settled({ type: "agent_settled" }, makeContext(100.6));
  await waitFor(async () => (await starts(logPath)).length >= 11, "question settle events missing");
  assert.deepEqual(parsedCalls((await starts(logPath)).slice(9, 11)), [
    { event: "session_stopped", payload: { last_message: "Which option should I use?" } },
    { event: "context_changed", payload: { fill_percent: 100 } },
  ]);

  input({ type: "input", text: "Error task", source: "interactive" }, makeContext());
  agentStart({ type: "agent_start" }, makeContext());
  messageEnd({
    type: "message_end",
    message: { role: "assistant", content: [], stopReason: "error", errorMessage: "provider failed" },
  }, makeContext());
  settled({ type: "agent_settled" }, makeContext(-4.8));
  await waitFor(async () => (await starts(logPath)).length >= 16, "error settle events missing");
  assert.deepEqual(parsedCalls((await starts(logPath)).slice(14, 16)), [
    { event: "state_changed", payload: { state: "error" } },
    { event: "context_changed", payload: { fill_percent: 0 } },
  ]);

  await shutdown(api);
  return starts(logPath);
}

// @lat: [[test#Test Harness#Pi Extension Harness#Malformed messages and no polling]]
async function testMalformedMessagesAndNoPolling() {
  const logPath = join(tempDir, "malformed.jsonl");
  setHarnessEnv(logPath);
  const realSetInterval = globalThis.setInterval;
  let intervalCalls = 0;
  globalThis.setInterval = (...args) => {
    intervalCalls += 1;
    return realSetInterval(...args);
  };
  try {
    const api = new FakeExtensionAPI();
    extensionFactory(api);
    const messageEnd = api.handler("message_end");
    assert.doesNotThrow(() => messageEnd({ type: "message_end", message: null }, makeContext()));
    assert.doesNotThrow(() => messageEnd({
      type: "message_end",
      message: { role: "assistant", content: [null, 4, { type: "text", text: 5 }], stopReason: "stop" },
    }, makeContext()));
    await new Promise((resolve) => setTimeout(resolve, 30));
    assert.equal((await starts(logPath)).length, 0, "message capture must not emit directly");
    api.handler("agent_settled")({ type: "agent_settled" }, makeContext(null));
    await waitFor(async () => (await starts(logPath)).length >= 1, "malformed settle did not remain live");
    assert.deepEqual(parsedCalls((await starts(logPath)).slice(0, 1)), [
      { event: "session_stopped", payload: { last_message: "" } },
    ]);
    assert.equal(intervalCalls, 0, "extension must not poll");
    assert.equal(api.count("tool_call"), 1, "tool calls feed issue focus only");
    await shutdown(api);
    return starts(logPath);
  } finally {
    globalThis.setInterval = realSetInterval;
  }
}

// @lat: [[test#Test Harness#Pi Extension Harness#Issue focus from a claim]]
async function testIssueFocusedFromBdClaim() {
  const logPath = join(tempDir, "issue-focused.jsonl");
  setHarnessEnv(logPath);
  const api = new FakeExtensionAPI();
  extensionFactory(api);
  const toolCall = api.handler("tool_call");

  const call = (input, toolName = "bash") =>
    toolCall({ type: "tool_call", toolCallId: "t1", toolName, input }, makeContext());

  // Observation must never block the tool or rewrite its arguments.
  const input = { command: "bd update scribe-lpi2.13 --claim" };
  assert.equal(call(input), undefined, "tool_call must not block or defer");
  assert.deepEqual(input, { command: "bd update scribe-lpi2.13 --claim" });
  await waitFor(async () => (await starts(logPath)).length >= 1, "claim did not emit");
  assert.deepEqual(parsedCalls(await starts(logPath)), [
    { event: "issue_focused", payload: { issue_id: "scribe-lpi2.13" } },
  ]);

  // Forms that must still resolve the id.
  call({ command: "cd /repo && BD_NO_DAEMON=1 /usr/local/bin/bd update scribe-lpi2.9 --claim" });
  call({ command: "bd --actor codex-implement-ready-run-20260818T085731.REeEi4 update bd-42 --claim" });
  await waitFor(async () => (await starts(logPath)).length >= 3, "chained/global-flag claims missing");
  assert.deepEqual(parsedCalls((await starts(logPath)).slice(1, 3)), [
    { event: "issue_focused", payload: { issue_id: "scribe-lpi2.9" } },
    { event: "issue_focused", payload: { issue_id: "bd-42" } },
  ]);

  // Nothing below is a claim, so none may emit.
  const before = (await starts(logPath)).length;
  for (const command of [
    "bd update scribe-lpi2.13",
    "bd list --all",
    "git commit -m 'bd update scribe-lpi2.13 --claim'",
    "echo --claim",
    "bdx update scribe-lpi2.13 --claim",
    "",
  ]) {
    call({ command });
  }
  call({ command: 123 });
  call({}, "bash");
  call({ command: "bd update scribe-lpi2.13 --claim" }, "read");
  await new Promise((resolve) => setTimeout(resolve, 60));
  assert.equal((await starts(logPath)).length, before, "non-claim commands must not emit");

  await shutdown(api);
  return starts(logPath);
}

// @lat: [[test#Test Harness#Pi Extension Harness#Callbacks do not await the helper]]
async function testCallbacksDoNotAwaitHelper() {
  const logPath = join(tempDir, "responsive.jsonl");
  setHarnessEnv(logPath, 500);
  const api = new FakeExtensionAPI();
  extensionFactory(api);
  const started = performance.now();
  const result = api.handler("agent_start")({ type: "agent_start" }, makeContext());
  const elapsed = performance.now() - started;
  assert.equal(result, undefined, "normal callbacks must not return helper promises");
  assert.ok(elapsed < 25, `callback blocked for ${elapsed.toFixed(1)} ms`);
  await waitFor(async () => (await starts(logPath)).length >= 1, "slow helper was not launched");
  await shutdown(api);
  return starts(logPath);
}

// @lat: [[test#Test Harness#Pi Extension Harness#Serial queue cap]]
async function testSerialQueueCap() {
  const logPath = join(tempDir, "queue.jsonl");
  setHarnessEnv(logPath, 10);
  const api = new FakeExtensionAPI();
  extensionFactory(api);
  const agentStart = api.handler("agent_start");
  for (let index = 0; index < 100; index += 1) {
    agentStart({ type: "agent_start" }, makeContext());
  }
  await waitFor(async () => {
    const value = await records(logPath);
    return value.filter((record) => record.phase === "end").length >= 32 ? value : null;
  }, "bounded queue did not drain");
  const log = await records(logPath);
  const processingStarts = log.filter((record) => record.phase === "start");
  assert.equal(processingStarts.length, 32, "queue must cap total outstanding events at 32");
  for (let index = 0; index < log.length; index += 2) {
    assert.equal(log[index]?.phase, "start", "serial helper must start before it ends");
    assert.equal(log[index + 1]?.phase, "end", "next helper must wait for prior close");
    assert.equal(log[index]?.pid, log[index + 1]?.pid);
  }
  await shutdown(api);
  return starts(logPath);
}

// @lat: [[test#Test Harness#Pi Extension Harness#Generation-cancelled shutdown and reload]]
async function testGenerationCancelledShutdownAndReload() {
  const logPath = join(tempDir, "shutdown.jsonl");
  setHarnessEnv(logPath, 60);
  const api = new FakeExtensionAPI();
  extensionFactory(api);
  const agentStart = api.handler("agent_start");
  for (let index = 0; index < 40; index += 1) {
    agentStart({ type: "agent_start" }, makeContext());
  }
  await waitFor(async () => (await starts(logPath)).length >= 1, "active helper did not start");
  const started = performance.now();
  await shutdown(api, "reload");
  const elapsed = performance.now() - started;
  assert.ok(elapsed < 250, `shutdown exceeded 250 ms: ${elapsed.toFixed(1)} ms`);
  await new Promise((resolve) => setTimeout(resolve, 150));
  let entries = await starts(logPath);
  assert.deepEqual(parsedCalls(entries), [
    { event: "state_changed", payload: { state: "processing" } },
    { event: "state_cleared", payload: {} },
  ]);

  process.env.FAKE_HELPER_SLEEP_MS = "0";
  const reloaded = new FakeExtensionAPI();
  extensionFactory(reloaded);
  reloaded.handler("session_start")({ type: "session_start", reason: "reload" }, makeContext());
  await waitFor(async () => (await starts(logPath)).length >= 4, "reload events missing");
  entries = await starts(logPath);
  assert.deepEqual(parsedCalls(entries.slice(2, 4)), [
    { event: "task_label_cleared", payload: {} },
    { event: "state_changed", payload: { state: "idle_prompt" } },
  ]);
  await shutdown(reloaded);
  return starts(logPath);
}

// Agent API tools: registration shape, argv contract, and the session gate.
async function testAgentToolContract() {
  const logPath = join(tempDir, "agent-tools.jsonl");
  setHarnessEnv(logPath);

  const cliDir = join(tempDir, "fake-cli-bin");
  const cliLog = join(tempDir, "agent-cli.jsonl");
  await mkdir(cliDir, { recursive: true });
  await writeFile(
    join(cliDir, "scribe"),
    `#!/usr/bin/env node
require("node:fs").appendFileSync(
  process.env.FAKE_AGENT_CLI_LOG,
  JSON.stringify(process.argv.slice(2)) + "\\n",
);
process.stdout.write('{"v":1,"ok":true,"data":{}}\\n');
`,
  );
  await chmod(join(cliDir, "scribe"), 0o755);

  const priorPath = process.env.PATH;
  process.env.PATH = `${cliDir}:${priorPath}`;
  process.env.FAKE_AGENT_CLI_LOG = cliLog;
  try {
    const api = new FakeExtensionAPI();
    extensionFactory(api);

    const expected = [
      "scribe_agent_action",
      "scribe_agent_capabilities",
      "scribe_agent_read",
      "scribe_agent_siblings",
      "scribe_agent_world",
      "scribe_agent_write",
    ];
    assert.deepEqual([...api.tools.keys()].sort(), expected, "typed agent tools must register");
    for (const name of expected) {
      const tool = api.tools.get(name);
      assert.ok(tool.label, `${name} needs a label`);
      assert.ok(tool.description, `${name} needs a description`);
      assert.equal(tool.parameters.type, "object", `${name} parameters must be an object schema`);
      assert.equal(typeof tool.execute, "function");
    }
    assert.deepEqual(api.tools.get("scribe_agent_read").parameters.required, ["session_id"]);
    assert.deepEqual(api.tools.get("scribe_agent_write").parameters.required, [
      "session_id",
      "text",
    ]);
    const actionSchema = api.tools.get("scribe_agent_action").parameters;
    assert.ok(
      actionSchema.properties.action.enum.includes("focus-session"),
      "action must be a typed enum",
    );

    // Execution shells to `scribe agent … --agent pi` and returns stdout.
    const read = await api.tools
      .get("scribe_agent_read")
      .execute("t1", { session_id: "sess-1", scrollback: 12 }, undefined, undefined, {});
    assert.deepEqual(read.content, [{ type: "text", text: '{"v":1,"ok":true,"data":{}}' }]);
    const write = await api.tools
      .get("scribe_agent_write")
      .execute("t2", { session_id: "sess-1", text: "echo hi", submit: true }, undefined, undefined, {});
    assert.deepEqual(write.content, [{ type: "text", text: '{"v":1,"ok":true,"data":{}}' }]);
    const argvLog = (await readFile(cliLog, "utf8")).trim().split("\n").map((line) => JSON.parse(line));
    assert.deepEqual(argvLog, [
      ["agent", "read", "sess-1", "--scrollback", "12", "--agent", "pi"],
      ["agent", "write", "sess-1", "--text", "echo hi", "--submit", "--agent", "pi"],
    ]);

    // A registered tool no-ops when SCRIBE_SESSION_ID is unset: no spawn,
    // explanatory text instead.
    delete process.env.SCRIBE_SESSION_ID;
    const blocked = await api.tools
      .get("scribe_agent_world")
      .execute("t3", {}, undefined, undefined, {});
    assert.match(blocked.content[0].text, /SCRIBE_SESSION_ID is unset/);
    assert.equal(
      (await readFile(cliLog, "utf8")).trim().split("\n").length,
      2,
      "a gated tool call must not spawn the CLI",
    );

    process.env.SCRIBE_SESSION_ID = randomUUID();
    await shutdown(api);
    return starts(logPath);
  } finally {
    process.env.PATH = priorPath;
    delete process.env.FAKE_AGENT_CLI_LOG;
  }
}

// @lat: [[test#Test Harness#Pi Extension Harness#Absent environment, child suppression, and missing helper]]
async function testAbsentEnvironmentHelperAndChildSuppression() {
  const absentLog = join(tempDir, "absent.jsonl");
  delete process.env.SCRIBE_HOOK_HELPER;
  delete process.env.SCRIBE_HOOK_SOCK;
  delete process.env.SCRIBE_SESSION_ID;
  delete process.env.PI_SUBAGENT_CHILD;
  process.env.FAKE_HELPER_LOG = absentLog;
  const absent = new FakeExtensionAPI();
  extensionFactory(absent);
  assert.equal(absent.handlers.size, 0, "absent Scribe environment must no-op");
  assert.equal(absent.tools.size, 0, "absent Scribe environment must register no tools");

  const childLog = join(tempDir, "child.jsonl");
  setHarnessEnv(childLog);
  process.env.PI_SUBAGENT_CHILD = "1";
  const child = new FakeExtensionAPI();
  extensionFactory(child);
  assert.equal(child.handlers.size, 0, "Pi child process must no-op");
  assert.equal(child.tools.size, 0, "Pi child process must register no tools");
  assert.equal((await starts(childLog)).length, 0);

  const missingLog = join(tempDir, "missing-helper.jsonl");
  setHarnessEnv(missingLog, 0, join(tempDir, "does-not-exist"));
  const missing = new FakeExtensionAPI();
  extensionFactory(missing);
  assert.doesNotThrow(() => missing.handler("session_start")(
    { type: "session_start", reason: "startup" },
    makeContext(),
  ));
  await new Promise((resolve) => setTimeout(resolve, 50));
  assert.equal((await starts(missingLog)).length, 0, "missing helper must fail silently");
  await shutdown(missing);
  return [];
}

const allStarts = [];
try {
  allStarts.push(...await testStartupAndDuplicateGuard());
  allStarts.push(...await testInputSourcesAndOrder());
  allStarts.push(...await testSharedQuestionnaireWait());
  allStarts.push(...await testRetryAndSettleBehavior());
  allStarts.push(...await testMalformedMessagesAndNoPolling());
  allStarts.push(...await testIssueFocusedFromBdClaim());
  allStarts.push(...await testCallbacksDoNotAwaitHelper());
  allStarts.push(...await testSerialQueueCap());
  allStarts.push(...await testGenerationCancelledShutdownAndReload());
  allStarts.push(...await testAgentToolContract());
  allStarts.push(...await testAbsentEnvironmentHelperAndChildSuppression());
  assertFixedArgv(allStarts);
  assertNoPermissionPrompt(allStarts);
  console.log("pi-extension harness: ok");
} finally {
  restoreEnv();
  await rm(tempDir, { recursive: true, force: true });
}

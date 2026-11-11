import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, mkdir, open, readFile, rename, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createHash, randomUUID } from "node:crypto";
import { fileURLToPath } from "node:url";
import { AcpAgent, AcpNotSubmittedError } from "../agent.mjs";
import { AcpBridge, limitedComputerTools, offersComputerUse } from "../bridge.mjs";
import { RECEIPT_LIMIT, Sessions } from "../sessions.mjs";
import { openCodeAdapter, cursorAdapter } from "../catalog.mjs";
import { qualifiedName, serverName } from "../tool-server.mjs";

const fake = fileURLToPath(new URL("./fake-agent.mjs", import.meta.url));
// The signal a peer that ignores EOF and SIGTERM ends with. Windows has no
// catchable SIGTERM: Node's kill("SIGTERM") already terminates the process.
const FORCED_EXIT = process.platform === "win32" ? "SIGTERM" : "SIGKILL";
// Renaming a journal onto a directory: POSIX reports EISDIR, Windows EPERM.
const JOURNAL_BLOCKED = process.platform === "win32" ? "EPERM" : "EISDIR";

// These provider-neutral ACP contracts use real child pipes and opaque session
// IDs, not the OpenCode CLI. Deadlines must include writing and shutdown,
// and a bad peer must not leave an accepted request waiting forever.
async function transport(t, options = {}) {
  const agent = new AcpAgent({ command: process.execPath, args: [fake], ...options });
  t.after(async () => {
    if (agent.child && agent.child.exitCode == null && agent.child.signalCode == null) {
      const exited = new Promise(resolve => agent.child.once("exit", resolve));
      agent.child.kill("SIGKILL");
      await exited;
    }
  });
  return agent;
}

async function within(promise, ms = 1500) {
  let timer;
  try {
    return await Promise.race([promise, new Promise((_, reject) => {
      timer = setTimeout(() => reject(new Error("test deadline exceeded")), ms);
    })]);
  } finally { clearTimeout(timer); }
}

test("a timed out request is retired while another request can finish", async t => {
  const agent = await transport(t);
  await agent.start();
  const stalled = agent.request("test/no-reply", {}, { timeoutMs: 40 });
  const independent = agent.request("test/delay", { text: "still working", delayMs: 80 }, { timeoutMs: 1000 });
  await assert.rejects(within(stalled), error => error.code === "ACP_REQUEST_TIMEOUT" && error.outcomeUnknown === true);
  assert.deepEqual(await independent, { echoed: "still working" });
  assert.equal(agent.pending.size, 0);
  assert.equal(agent.alive, true);
});

test("a prompt gets its own deadline and expiration cancels only its session", async t => {
  const agent = await transport(t, { requestTimeoutMs: 30, promptTimeoutMs: 200 });
  await agent.start();
  // The delayed peer replies after an ordinary request would have timed out.
  const prompt = agent.request("session/prompt", { sessionId: "long", prompt: [{ type: "text", text: "wait" }] });
  await new Promise(resolve => setTimeout(resolve, 60));
  assert.equal(agent.pending.size, 1);
  await agent.notify("session/cancel", { sessionId: "long" });
  assert.equal((await prompt).stopReason, "cancelled");
  await assert.rejects(within(agent.request("test/no-reply", {}, { timeoutMs: 40 })), /confirm/);
  // A peer which ignores a prompt remains an uncertain outcome, not success.
  const stalled = agent.request("session/prompt", { sessionId: "expired", prompt: [{ type: "text", text: "wait" }] }, { timeoutMs: 40 });
  await assert.rejects(within(stalled), error => error.code === "ACP_REQUEST_TIMEOUT" && error.outcomeUnknown === true);
  const cancelled = await agent.request("test/cancelled-sessions", {});
  assert.deepEqual(cancelled.sessions.sort(), ["expired", "long"]);
  assert.equal(agent.alive, true);
  assert.equal(agent.pending.size, 0);
});

// Node on Windows reads a child's piped stdin with a blocking read it cannot
// cancel, so the fake peer cannot close its input while staying alive.
test("a timed out prompt preserves an unsuccessful cancellation as an uncertain outcome", {
  skip: process.platform === "win32" && "a Windows peer cannot close its stdin pipe while a read is pending",
}, async t => {
  const agent = await transport(t, { env: { WONDER_FAKE_ACP_FAULT: "close-input-on-prompt" } });
  await agent.start();
  await assert.rejects(within(agent.request("session/prompt", {
    sessionId: "unconfirmed", prompt: [{ type: "text", text: "wait" }],
  }, { timeoutMs: 60 })), error => error.outcomeUnknown === true && /receiving requests/.test(error.cause?.message));
  assert.equal(agent.pending.size, 0);
  assert.equal(agent.alive, false);
});

for (const fault of ["malformed", "null", "invalid-response", "oversized"]) {
  test(`a ${fault} frame fails the connection instead of losing the reply`, async t => {
    const agent = await transport(t);
    await agent.start();
    await assert.rejects(within(agent.request(`test/${fault}`, {}), 3000), /invalid|too large/i);
    assert.equal(agent.alive, false);
    assert.equal(agent.pending.size, 0);
  });
}

test("a blocked input pipe has a write deadline and retires pending requests", async t => {
  const agent = await transport(t, { writeTimeoutMs: 40 });
  await agent.start();
  await agent.request("test/pause-input", {});
  await assert.rejects(within(agent.request("test/delay", { text: "x".repeat(8 * 1024 * 1024), delayMs: 0 })), error => {
    assert.match(error.message, /receiving requests/);
    assert.equal(error instanceof AcpNotSubmittedError, false, "a partial write cannot be retried safely");
    return true;
  });
  assert.equal(agent.pending.size, 0);
  assert.equal(agent.alive, false);
});

test("an unsendable frame fails only that request without leaking its receipt", async t => {
  const agent = await transport(t);
  await agent.start();
  const circular = {}; circular.self = circular;
  await assert.rejects(agent.request("test/delay", circular), error => error instanceof AcpNotSubmittedError && /circular/i.test(error.message));
  assert.equal(agent.pending.size, 0);
  assert.deepEqual(await agent.request("test/delay", { text: "next", delayMs: 0 }), { echoed: "next" });
});

test("shutdown kills the owned peer if it ignores EOF and SIGTERM", async t => {
  const other = await transport(t);
  await other.start();
  const agent = await transport(t, { env: { WONDER_FAKE_ACP_FAULT: "ignore-close" } });
  await agent.start();
  const pending = assert.rejects(agent.request("test/no-reply", {}), /closed/);
  await within(Promise.all([agent.close({ graceMs: 30, killMs: 30 }), pending]));
  assert.equal(agent.child.signalCode, FORCED_EXIT);
  assert.equal(agent.pending.size, 0);
  await assert.rejects(agent.notify("session/cancel", { sessionId: "late" }), /not running/);
  assert.deepEqual(await other.request("test/delay", { text: "unaffected", delayMs: 0 }), { echoed: "unaffected" });
});

test("initialization has a deadline even when the peer ignores SIGTERM", async t => {
  const agent = await transport(t, { env: { WONDER_FAKE_ACP_FAULT: "no-initialize" } });
  await assert.rejects(within(agent.start({ timeoutMs: 40 }), 5000), /confirm/);
  assert.equal(agent.child.signalCode, FORCED_EXIT);
  assert.equal(agent.pending.size, 0);
});

async function harness({ adapter = openCodeAdapter, approve = "accept", nativePermission = "ask", fault, promptTimeoutMs, requestTimeoutMs, stopTimeoutMs, idleMs, commandScript, permissionOptions, sendGate, outputLimits } = {}) {
  const root = await mkdtemp(join(tmpdir(), "wonder-acp-"));
  const frames = [], agents = [];
  let bridge;
  const send = async frame => {
    if (sendGate) await sendGate(frame);
    frames.push(frame);
    if (approve !== null && frame.method?.endsWith("/requestApproval")) setImmediate(() => bridge.receive({ id: frame.id, result: { decision: approve } }));
  };
  const sessions = await new Sessions(join(root, "sessions"), adapter).initialize();
  const errors = [];
  const bridgeOptions = { adapter, send, outputLimits, onFatal: error => errors.push(error), ...(stopTimeoutMs ? { stopTimeoutMs } : {}), ...(idleMs ? { idleMs } : {}),
    inspect: async () => ({ installed: true, version: "1.0.0", models: [{ id: "opencode/free-model", name: "Free Model", providerId: "opencode", free: true }], defaultModel: null }),
    createAgent: handlers => {
      const peerFault = Array.isArray(fault) ? fault[agents.length] : fault;
      const agent = new AcpAgent({ command: peerFault === "missing-executable" ? join(root, "missing-opencode") : process.execPath, args: [fake], env: { WONDER_FAKE_ACP_PROVIDER: adapter.family, WONDER_FAKE_NATIVE_PERMISSION: nativePermission, ...(permissionOptions ? { WONDER_FAKE_ACP_PERMISSION_OPTIONS: JSON.stringify(permissionOptions) } : {}), ...(commandScript ? { WONDER_FAKE_ACP_COMMANDS: JSON.stringify(commandScript) } : {}), ...(peerFault ? { WONDER_FAKE_ACP_FAULT: peerFault } : {}) },
        ...(promptTimeoutMs ? { promptTimeoutMs } : {}), ...(requestTimeoutMs ? { requestTimeoutMs } : {}), ...handlers });
      agents.push(agent); return agent;
    } };
  bridge = new AcpBridge({ ...bridgeOptions, sessions });
  const call = async (method, params) => {
    const id = `t-${frames.length}-${Math.random()}`;
    await bridge.receive({ id, method, params });
    const reply = frames.find(f => f.id === id && !f.method);
    if (reply.error) throw new Error(reply.error.message);
    return reply.result;
  };
  const turnDone = turnId => new Promise((resolve, reject) => {
    const started = Date.now();
    const poll = () => {
      const done = frames.find(f => f.method === "turn/completed" && f.params.turnId === turnId);
      if (done) return resolve(done.params.turn);
      if (Date.now() - started > 10_000) return reject(new Error("turn did not complete"));
      setTimeout(poll, 10);
    };
    poll();
  });
  const start = (policy = { mode: "workspace", approvalMode: "ask" }, requestId = randomUUID()) => call("thread/start", { requestId, cwd: root, model: adapter.family === "cursor" ? "cursor:gpt-5.6-luna[context=272k,reasoning=medium,fast=false]" : "opencode:opencode/free-model", wonderPolicy: policy, wonderProject: { cwd: root, additionalDirectories: [] } });
  return { root, frames, agents, errors, get bridge() { return bridge; }, call, turnDone, start,
    restart: async () => { await bridge.close(); bridge = new AcpBridge({ ...bridgeOptions, sessions: await new Sessions(join(root, "sessions"), adapter).initialize() }); },
    cleanup: async () => { await bridge.close(); await rm(root, { recursive: true, force: true }); } };
}

test("a command with an image fails before native submission and retains its input", async () => {
  const h = await harness();
  try {
    const { thread } = await h.start();
    const bytes = Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]);
    const path = join(h.root, "photo.png");
    await writeFile(path, bytes);
    const image = { type: "localImage", path, mimeType: "image/png", byteSize: bytes.length, sha256: createHash("sha256").update(bytes).digest("hex") };
    const input = [{ type: "text", text: "/review" }, image];
    const params = { threadId: thread.id, clientUserMessageId: "command-image", input };
    const { turn } = await h.call("turn/start", params);
    const done = await h.turnDone(turn.id);
    assert.equal(done.status, "failed");
    assert.match(done.error.message, /Send the command and attachments separately/);
    assert.equal((await h.agents[0].request("test/state", {})).prompts.length, 0);
    const saved = await new Sessions(join(h.root, "sessions")).initialize();
    assert.deepEqual(saved.get(thread.id).turns[0].items[0].content, input);
    const again = await h.call("turn/start", params);
    assert.equal(again.turn.id, turn.id);
    assert.equal(again.turn.status, "failed");
    const retry = await h.call("turn/start", { ...params, clientUserMessageId: "edited-image", input: [{ type: "text", text: "Review this" }, image] });
    assert.equal((await h.turnDone(retry.turn.id)).status, "completed");
    assert.equal((await h.agents[0].request("test/state", {})).prompts.length, 1);
  } finally { await h.cleanup(); }
});

test("a reply streams as one agent message and the turn completes", async () => {
  const h = await harness();
  try {
    const { thread } = await h.start();
    assert.match(thread.id, /^opencode-/);
    assert.match(thread.sessionId, /^ses_/);
    const { turn } = await h.call("turn/start", { threadId: thread.id, clientUserMessageId: "m1", input: [{ type: "text", text: "hi" }] });
    const done = await h.turnDone(turn.id);
    assert.equal(done.status, "completed");
    const message = done.items.find(i => i.type === "agentMessage");
    assert.equal(message.text, "Hello there");
    const deltas = h.frames.filter(f => f.method === "item/agentMessage/delta").map(f => f.params.delta);
    assert.deepEqual(deltas, ["Hello", " there"]);
    assert.ok(h.frames.some(f => f.method === "thread/tokenUsage/updated" && f.params.tokenUsage.modelContextWindow === 200000));
    // A resent message returns the same receipt and runs nothing.
    const again = await h.call("turn/start", { threadId: thread.id, clientUserMessageId: "m1", input: [{ type: "text", text: "hi" }] });
    assert.equal(again.turn.id, turn.id);
    const history = await h.call("thread/turns/list", { threadId: thread.id });
    assert.equal(history.data.length, 1);
    // The provider's thinking is saved with the reply, ahead of its text.
    assert.deepEqual(history.data[0].items.map(i => i.type), ["userMessage", "reasoning", "agentMessage"]);
    assert.deepEqual(history.data[0].items.find(i => i.type === "reasoning"), { type: "reasoning", id: `${turn.id}:thinking:1`, status: "completed", summary: ["Hmm"] });
  } finally { await h.cleanup(); }
});

test("a command asks for approval, then runs; text before it is commentary", async () => {
  const h = await harness({ approve: "accept" });
  try {
    const { thread } = await h.start();
    const { turn } = await h.call("turn/start", { threadId: thread.id, input: [{ type: "text", text: "run a command" }] });
    const done = await h.turnDone(turn.id);
    const approval = h.frames.find(f => f.method === "item/commandExecution/requestApproval");
    assert.equal(approval.params.command, "ls");
    assert.equal(approval.params.kind, "command");
    assert.equal(approval.params.cwd, null, "A missing command folder cannot borrow the session folder for notification consent");
    assert.equal(approval.params.itemId, "call-1");
    assert.deepEqual(approval.params.availableDecisions, ["accept", { opencodePermission: { optionId: "always", kind: "allow_always" } }, "decline", "cancel"]);
    const command = done.items.find(i => i.type === "commandExecution");
    assert.equal(command.status, "completed");
    assert.equal(command.aggregatedOutput, "a.txt\n");
    assert.equal(done.items.find(i => i.text === "Checking.").phase, "commentary");
    assert.equal(done.items.find(i => i.text === "Done.").phase, undefined);
    assert.deepEqual(done.items.find(i => i.type === "taskChecklist").taskChecklist.entries, [{ text: "List files", status: "completed" }]);
    assert.equal(done.items.some(i => i.type === "plan"), false);
  } finally { await h.cleanup(); }
});

// Observable contract: the real ACP child receives only its advertised ID;
// remembered denial never takes the fake provider's allow branch.
const remembered = (optionId, kind) => ({ opencodePermission: { optionId, kind } });
for (const [kind, decision, expected, allowed] of [
  ["allow_once", "accept", "once", true],
  ["reject_once", "decline", "reject", false],
  ["allow_always", remembered("provider/allow:α", "allow_always"), "provider/allow:α", true],
  ["reject_always", remembered("provider/deny:β", "reject_always"), "provider/deny:β", false],
  ["cancel", "cancel", null, false],
  ["unknown", remembered("forged", "allow_always"), null, false],
  ["session is not always", "acceptForSession", null, false],
  ["wrong-kind", remembered("provider/deny:β", "allow_always"), null, false],
]) {
  test(`ACP permission round trip: ${kind}`, async () => {
    const permissionOptions = [
      { optionId: "once", kind: "allow_once" }, { optionId: "reject", kind: "reject_once" },
      { optionId: "provider/allow:α", kind: "allow_always" }, { optionId: "provider/deny:β", kind: "reject_always" },
    ];
    const h = await harness({ approve: decision, permissionOptions });
    try {
      const { thread } = await h.start();
      const { turn } = await h.call("turn/start", { threadId: thread.id, input: [{ type: "text", text: "run a command" }] });
      const done = await h.turnDone(turn.id);
      const state = await h.agents[0].request("test/state", {});
      assert.deepEqual(state.permissionAnswers, [expected == null ? { outcome: "cancelled" } : { outcome: "selected", optionId: expected }]);
      assert.equal(done.items.find(i => i.type === "commandExecution").status, allowed ? "completed" : "failed");
    } finally { await h.cleanup(); }
  });
}

for (const [label, options, decision, expected] of [
  ["always allow only", [{ optionId: "allow", kind: "allow_always" }], remembered("allow", "allow_always"), "allow"],
  ["always decline only", [{ optionId: "deny", kind: "reject_always" }], remembered("deny", "reject_always"), "deny"],
  ["cancel always only", [{ optionId: "allow", kind: "allow_always" }], "cancel", null],
  ["no fabricated once", [{ optionId: "allow", kind: "allow_always" }], "accept", null],
  ["no fabricated decline", [{ optionId: "deny", kind: "reject_always" }], "decline", null],
  ["missing kind", [{ optionId: "bad" }], "accept", null],
  ["malformed kind", [{ optionId: "bad", kind: ["allow_always"] }], remembered("bad", "allow_always"), null],
  ["unknown kind", [{ optionId: "bad", kind: "allow_everything" }], "accept", null],
  ["empty ID", [{ optionId: "", kind: "allow_always" }], remembered("", "allow_always"), null],
  ["duplicate ID", [{ optionId: "same", kind: "allow_always" }, { optionId: "same", kind: "reject_always" }], remembered("same", "allow_always"), null],
  ["duplicate ID with unknown kind", [{ optionId: "same", kind: "allow_always" }, { optionId: "same", kind: "future" }], remembered("same", "allow_always"), null],
  ["duplicate ID and kind", [{ optionId: "same", kind: "allow_always" }, { optionId: "same", kind: "allow_always" }], remembered("same", "allow_always"), null],
  ["missing options", null, "accept", null],
  ["malformed options", {}, "accept", null],
  ["malformed row", [null], "accept", null],
  ["unknown row cannot hide valid denial", [{ optionId: "bad", kind: "future" }, { optionId: "deny", kind: "reject_always" }], remembered("deny", "reject_always"), "deny"],
]) {
  test(`ACP permission option validation: ${label}`, async () => {
    const h = await harness({ approve: decision });
    try {
      const { thread } = await h.start();
      const { turn } = await h.call("turn/start", { threadId: thread.id, input: [{ type: "text", text: "wait" }] });
      await until(() => h.frames.some(frame => frame.method === "item/agentMessage/delta"));
      const result = await h.agents[0].onPermission({ ...permissionRequest(thread.sessionId), options });
      assert.deepEqual(result, expected == null ? { outcome: "cancelled" } : { outcome: "selected", optionId: expected });
      const approval = h.frames.find(frame => frame.method?.endsWith("/requestApproval"));
      if (approval) {
        assert.equal(approval.params.availableDecisions.includes("accept"), false);
        assert.equal(approval.params.availableDecisions.includes("decline"), false);
        assert.equal(approval.params.availableDecisions.at(-1), "cancel");
      }
      await h.call("turn/interrupt", { threadId: thread.id, turnId: turn.id });
    } finally { await h.cleanup(); }
  });
}

test("Cupola tool handling never chooses a remembered provider rule automatically", async () => {
  const h = await harness();
  try {
    const { thread } = await h.start();
    const { turn } = await h.call("turn/start", { threadId: thread.id, input: [{ type: "text", text: "wait" }] });
    await until(() => h.frames.some(frame => frame.method === "item/agentMessage/delta"));
    for (const server of [serverName(thread.id), serverName("another-thread")]) {
      const request = permissionRequest(thread.sessionId);
      request.toolCall.title = qualifiedName(server, "wonder_thread_read");
      request.options = [{ optionId: "always", kind: "allow_always" }, { optionId: "never", kind: "reject_always" }];
      assert.deepEqual(await h.agents[0].onPermission(request), { outcome: "cancelled" });
    }
    assert.equal(h.frames.some(frame => frame.method?.endsWith("/requestApproval")), false);
    await h.call("turn/interrupt", { threadId: thread.id, turnId: turn.id });
  } finally { await h.cleanup(); }
});

for (const kind of ["edit", "read"]) {
  test(`remembered approval for an ACP ${kind} preserves the exact second same-kind option`, async () => {
    const decision = remembered("second", "reject_always");
    const h = await harness({ approve: decision });
    try {
      const { thread } = await h.start();
      const { turn } = await h.call("turn/start", { threadId: thread.id, input: [{ type: "text", text: "wait" }] });
      await until(() => h.frames.some(frame => frame.method === "item/agentMessage/delta"));
      const request = permissionRequest(thread.sessionId);
      request.toolCall = { ...request.toolCall, kind, locations: [{ path: "/tmp/a.txt" }] };
      request.options = [{ optionId: "first", kind: "reject_always" }, { optionId: "second", kind: "reject_always" }];
      assert.deepEqual(await h.agents[0].onPermission(request), { outcome: "selected", optionId: "second" });
      const approval = h.frames.find(frame => frame.method?.endsWith("/requestApproval"));
      assert.equal(approval.method, kind === "edit" ? "item/fileChange/requestApproval" : "item/commandExecution/requestApproval");
      assert.deepEqual(approval.params.availableDecisions, [remembered("first", "reject_always"), decision, "cancel"]);
      await h.call("turn/interrupt", { threadId: thread.id, turnId: turn.id });
    } finally { await h.cleanup(); }
  });
}

for (const trigger of ["Stop", "agent exit"]) {
  for (const kind of ["allow_always", "reject_always"]) {
    test(`a remembered ${kind} response arriving after actual ${trigger} cannot select a provider option`, async () => {
      const h = await harness({ approve: null, permissionOptions: [
        { optionId: "always", kind: "allow_always" }, { optionId: "never", kind: "reject_always" },
      ] });
      try {
        const { thread } = await h.start();
        const { turn } = await h.call("turn/start", { threadId: thread.id, input: [{ type: "text", text: "run a command" }] });
        await until(() => h.frames.some(frame => frame.method?.endsWith("/requestApproval")));
        const approval = h.frames.find(frame => frame.method?.endsWith("/requestApproval"));
        if (trigger === "Stop") await h.call("turn/interrupt", { threadId: thread.id, turnId: turn.id });
        else {
          const exited = new Promise(resolve => h.agents[0].child.once("exit", resolve));
          h.agents[0].child.kill("SIGKILL");
          await exited;
        }
        await until(() => !h.bridge.pending.has(approval.id));
        await h.bridge.receive({ id: approval.id, result: { decision: remembered(kind === "allow_always" ? "always" : "never", kind) } });
        const done = await h.turnDone(turn.id);
        // The fake provider normally completes after its permission is
        // cancelled; that native terminal result stays authoritative.
        assert.equal(done.status, trigger === "Stop" ? "completed" : "failed");
        assert.notEqual(done.items.find(item => item.type === "commandExecution")?.status, "completed");
        if (trigger === "Stop") {
          const state = await h.agents[0].request("test/state", {});
          assert.deepEqual(state.permissionAnswers, [{ outcome: "cancelled" }]);
        }
        assert.equal(h.bridge.pending.size, 0);
      } finally { await h.cleanup(); }
    });
  }
}

test("native tool images survive live delivery and reopening saved history", async () => {
  const h = await harness();
  try {
    const { thread } = await h.start();
    const { turn } = await h.call("turn/start", { threadId: thread.id, input: [{ type: "text", text: "tool images" }] });
    const done = await h.turnDone(turn.id);
    await h.bridge.sessions.flush();
    const history = await h.call("thread/items/list", { threadId: thread.id, turnId: turn.id });
    const reopened = await new Sessions(join(h.root, "sessions")).initialize();
    const live = h.frames.filter(f => f.method === "item/completed").map(f => f.params.item);
    for (const items of [live, done.items, history.data.map(entry => entry.item), reopened.get(thread.id).turns[0].items]) {
      for (const [kind, type] of [["read", "mcpToolCall"], ["execute", "commandExecution"], ["edit", "fileChange"]]) {
        const item = items.find(i => i.id === `image-${kind}`);
        assert.equal(item.type, type);
        assert.equal(item.status, kind === "edit" ? "failed" : "completed");
        assert.deepEqual(item.contentItems?.filter(c => c.type === "image"),
          [{ type: "image", mimeType: "image/png", data: "aW1hZ2UgYnl0ZXM=" }], `${kind} must retain exactly one native image`);
      }
    }
    assert.equal(done.items.find(i => i.type === "commandExecution").aggregatedOutput, "Preview");
    assert.equal(done.items.find(i => i.type === "fileChange").error.message, "Preview");
    assert.equal(done.items.find(i => i.type === "mcpToolCall").contentItems[0].text, "Preview");
  } finally { await h.cleanup(); }
});

test("notification consent keeps exact command context and rejects title fallbacks", async () => {
  const h = await harness();
  try {
    const { thread } = await h.start();
    await h.call("turn/start", { threadId: thread.id, input: [{ type: "text", text: "wait" }] });
    await until(() => h.frames.some(frame => frame.method === "item/agentMessage/delta"));
    for (const [rawInput, kind, cwd] of [[{ command: "pwd", workdir: "/workspace/actual" }, "command", "/workspace/actual"], [{}, "tool", null]]) {
      const request = permissionRequest(thread.sessionId);
      request.toolCall = { ...request.toolCall, toolCallId: `command-${kind}`, title: "display title", kind: "execute", rawInput };
      const outcome = await h.agents[0].onPermission(request);
      assert.equal(outcome.outcome, "selected");
      const approval = h.frames.filter(f => f.method === "item/commandExecution/requestApproval").at(-1);
      assert.equal(approval.params.kind, kind);
      assert.equal(approval.params.cwd, cwd);
    }
    await h.call("turn/interrupt", { threadId: thread.id, turnId: h.bridge.active.get(thread.id).turn.id });
  } finally { await h.cleanup(); }
});

// The phone lists every argument of a non-command tool in words and offers
// the exact input as Full details, so a long input arrives whole: a
// destination in the middle of a long value is never shortened away.
test("a long tool approval keeps the exact input", async () => {
  const h = await harness();
  try {
    const { thread } = await h.start();
    await h.call("turn/start", { threadId: thread.id, input: [{ type: "text", text: "wait" }] });
    await until(() => h.frames.some(frame => frame.method === "item/agentMessage/delta"));
    const url = `https://example.com/upload?a=${"a".repeat(600)}&destination=external&b=${"b".repeat(600)}`;
    const rawInput = { url, method: "POST", headers: { authorization: "Bearer x" }, body: "x".repeat(5000) };
    const request = permissionRequest(thread.sessionId);
    request.toolCall = { toolCallId: "fetch-long", title: "webfetch", kind: "fetch", rawInput };
    assert.equal((await h.agents[0].onPermission(request)).outcome, "selected");
    const approval = h.frames.filter(f => f.method === "item/commandExecution/requestApproval").at(-1);
    assert.equal(approval.params.kind, "tool");
    assert.ok(approval.params.command.startsWith("webfetch: "));
    assert.deepEqual(JSON.parse(approval.params.command.slice("webfetch: ".length)), rawInput, "No argument is shortened or dropped");
    await h.call("turn/interrupt", { threadId: thread.id, turnId: h.bridge.active.get(thread.id).turn.id });
  } finally { await h.cleanup(); }
});

test("a declined command fails without running", async () => {
  const h = await harness({ approve: "decline" });
  try {
    const { thread } = await h.start();
    const { turn } = await h.call("turn/start", { threadId: thread.id, input: [{ type: "text", text: "run a command" }] });
    const done = await h.turnDone(turn.id);
    assert.equal(done.items.find(i => i.type === "commandExecution").status, "failed");
  } finally { await h.cleanup(); }
});

// Permissions stay native, in Plan too: Plan is a work mode, not read-only.
// When the agent's own rules allow a command, it runs with no approval card,
// and Cupola adds no gate it cannot enforce.
for (const [adapter, planMode] of [[openCodeAdapter, false], [openCodeAdapter, true], [cursorAdapter, false], [cursorAdapter, true]]) {
  test(`the configured native allow policy runs without a Cupola approval (${adapter.label}${planMode ? ", Plan" : ""})`, async () => {
    const h = await harness({ adapter, nativePermission: "allow" });
    try {
      const { thread } = await h.start({ mode: "workspace", approvalMode: "ask", planMode });
      const { turn } = await h.call("turn/start", { threadId: thread.id, input: [{ type: "text", text: "run a command" }] });
      const done = await h.turnDone(turn.id);
      assert.equal(done.items.find(i => i.type === "commandExecution").status, "completed");
      assert.equal(h.frames.filter(f => f.method?.endsWith("/requestApproval")).length, 0);
      const { prompts } = await h.agents[0].request("test/state", {});
      assert.deepEqual(prompts.map(prompt => prompt.mode), [adapter.workModes[planMode ? "plan" : "default"]]);
    } finally { await h.cleanup(); }
  });
}

test("an edit becomes a file change with its diff", async () => {
  const h = await harness();
  try {
    const { thread } = await h.start();
    const { turn } = await h.call("turn/start", { threadId: thread.id, input: [{ type: "text", text: "edit it" }] });
    const done = await h.turnDone(turn.id);
    const change = done.items.find(i => i.type === "fileChange");
    assert.deepEqual(change.changes, [{ path: "/tmp/a.txt", kind: { type: "update" }, diff: "@@ @@\n-hi\n+bye" }]);
  } finally { await h.cleanup(); }
});

for (const [fault, outcome] of [[undefined, "interrupted"], ["failure-during-stop", "failed"], ["completion-during-stop", "completed"]]) {
  test(`Stop preserves the native ${outcome} outcome`, async () => {
    const h = await harness({ fault });
    try {
      const { thread } = await h.start();
      const { turn } = await h.call("turn/start", { threadId: thread.id, input: [{ type: "text", text: "wait please" }] });
      await until(() => h.frames.some(f => f.method === "item/agentMessage/delta"));
      assert.deepEqual((await within(h.call("turn/interrupt", { threadId: thread.id, turnId: turn.id }))).turn, { id: turn.id, status: outcome });
      const done = await h.turnDone(turn.id);
      assert.equal(done.status, outcome);
      if (outcome === "failed") assert.equal(done.error.message, "Native prompt failed during Stop.");
      // The conversation accepts the next message without replacing its peer.
      const next = await h.call("turn/start", { threadId: thread.id, input: [{ type: "text", text: "hi" }] });
      assert.equal((await h.turnDone(next.turn.id)).status, "completed");
      assert.equal(h.agents.length, 1);
    } finally { await h.cleanup(); }
  });
}

// Contract: a native reply beyond the former 3 s Stop window can still confirm
// within the phone's 15 s request budget. Real subprocess stdout identifies the
// original prompt; neither the cancel write nor test/release-cancel is evidence
// of its outcome. Existing Stop tests own setup, failures and terminal races.
for (const [stopReason, outcome] of [["cancelled", "interrupted"], ["end_turn", "completed"]]) {
  test(`Stop waits beyond 3 seconds for native ${outcome} confirmation`, async t => {
    const h = await harness({ fault: "held-cancel", promptTimeoutMs: 60_000 });
    try {
      const { thread } = await h.start();
      let stdout = "";
      h.agents[0].child.stdout.on("data", chunk => { stdout += chunk.toString(); });
      const { turn } = await h.call("turn/start", { threadId: thread.id, input: [{ type: "text", text: "wait" }] });
      await until(() => h.frames.some(frame => frame.method === "item/agentMessage/delta"));
      const prompt = (await h.agents[0].request("test/state", {})).prompts[0];
      const params = { threadId: thread.id, turnId: turn.id };
      const started = performance.now();
      const stopElapsedMs = [];
      const stop = async () => {
        try { return await h.call("turn/interrupt", params); }
        finally { stopElapsedMs.push(Math.round(performance.now() - started)); }
      };
      const stops = Promise.allSettled([stop(), stop()]);
      await until(async () => (await h.agents[0].request("test/state", {})).controls.some(control => control.method === "session/cancel"));
      await new Promise(resolve => setTimeout(resolve, 3500));
      assert.equal(h.frames.some(frame => frame.method === "turn/completed" && frame.params.turnId === turn.id), false);
      await h.agents[0].request("test/release-cancel", { sessionId: thread.sessionId, stopReason });
      const results = await stops;
      const elapsedMs = Math.round(performance.now() - started);
      const done = await h.turnDone(turn.id);
      await until(() => !h.bridge.active.has(thread.id));
      const native = stdout.trim().split("\n").map(line => JSON.parse(line));
      t.diagnostic(JSON.stringify({ elapsedMs, stopElapsedMs, prompt, stops: results.map(result => result.status === "fulfilled" ? result.value : result.reason.message),
        native: native.filter(frame => frame.method === "session/update" || frame.id === prompt.requestId) }));
      assert.deepEqual(native.find(frame => frame.id === prompt.requestId)?.result, { stopReason });
      assert.equal(done.status, outcome);
      assert.equal(done.items.find(item => item.type === "agentMessage").text, "Working — stopped");
      for (const result of results) {
        assert.equal(result.status, "fulfilled", "a timely native prompt reply must confirm Stop after 3 s");
        assert.deepEqual(result.value.turn, { id: turn.id, status: outcome });
      }
      assert.ok(elapsedMs >= 3500 && elapsedMs < 12_000, `Stop took ${elapsedMs} ms`);
      assert.deepEqual((await h.call("turn/interrupt", params)).turn, { id: turn.id, status: outcome });
      const state = await h.agents[0].request("test/state", {});
      assert.equal(state.controls.filter(control => control.method === "session/cancel" && control.sessionId === thread.sessionId).length, 1);
      assert.equal(h.frames.filter(frame => frame.method === "turn/completed" && frame.params.turnId === turn.id).length, 1);
      const saved = await new Sessions(join(h.root, "sessions")).initialize();
      assert.equal(saved.get(thread.id).turns[0].status, outcome);
      assert.equal(saved.get(thread.id).turns[0].items.find(item => item.type === "agentMessage").text, "Working — stopped");
      assert.equal(saved.get(thread.id).unresolvedTurnId, undefined);
      const next = await h.call("turn/start", { threadId: thread.id, input: [{ type: "text", text: "hi" }] });
      assert.equal((await h.turnDone(next.turn.id)).status, "completed");
      assert.equal(h.agents.length, 1);
    } finally { await h.cleanup(); }
  });
}

test("the default Stop deadline bounds an unresponsive native prompt without killing peers", async t => {
  const h = await harness({ fault: "ignore-cancel", promptTimeoutMs: 60_000 });
  try {
    const { thread } = await h.start();
    const { turn } = await h.call("turn/start", { threadId: thread.id, input: [{ type: "text", text: "wait" }] });
    await until(() => h.frames.some(frame => frame.method === "item/agentMessage/delta"));
    const started = performance.now();
    const stops = Promise.allSettled([h.call("turn/interrupt", { threadId: thread.id, turnId: turn.id }),
      h.call("turn/interrupt", { threadId: thread.id, turnId: turn.id })]);
    // The shared provider can finish another thread while Stop is waiting.
    const peer = (await h.start()).thread;
    const other = await h.call("turn/start", { threadId: peer.id, input: [{ type: "text", text: "hi" }] });
    assert.equal((await h.turnDone(other.turn.id)).status, "completed");
    const results = await within(stops, 12_000);
    const elapsedMs = Math.round(performance.now() - started);
    t.diagnostic(JSON.stringify({ elapsedMs, stops: results.map(result => result.reason?.message) }));
    assert.ok(elapsedMs >= 9000 && elapsedMs < 12_000, `Stop took ${elapsedMs} ms`);
    for (const result of results) {
      assert.equal(result.status, "rejected");
      assert.match(result.reason.message, /has not confirmed/);
    }
    await h.bridge.sessions.flush();
    assert.equal(h.bridge.sessions.get(thread.id).unresolvedTurnId, turn.id);
    assert.equal(h.bridge.active.has(thread.id), true);
    assert.equal(h.frames.some(frame => frame.method === "turn/completed" && frame.params.turnId === turn.id), false);
    await assert.rejects(h.call("turn/start", { threadId: thread.id, input: [{ type: "text", text: "overlap" }] }), /has not confirmed/);
    const saved = await new Sessions(join(h.root, "sessions")).initialize();
    assert.equal(saved.get(thread.id).unresolvedTurnId, turn.id);
    assert.equal(saved.describe(saved.get(thread.id)).canAcceptDirectInput, false);
    const state = await h.agents[0].request("test/state", {});
    assert.equal(state.controls.filter(control => control.method === "session/cancel" && control.sessionId === thread.sessionId).length, 1);
    assert.equal(h.agents[0].alive, true);
    assert.equal(h.agents.length, 1);
    const next = await h.call("turn/start", { threadId: peer.id, input: [{ type: "text", text: "hi again" }] });
    assert.equal((await h.turnDone(next.turn.id)).status, "completed");
  } finally { await h.cleanup(); }
});

// The native prompt response, including one arriving after a deadline, owns
// completion. A cancel notification itself never releases admission.
for (const trigger of ["prompt deadline", "user Stop"]) {
  test(`a late ACP stop confirmation after ${trigger} releases only its original turn`, async () => {
    const h = await harness({ fault: "held-cancel", promptTimeoutMs: trigger === "prompt deadline" ? 60 : 60_000, stopTimeoutMs: 60 });
    try {
      const { thread } = await h.start();
      const params = { threadId: thread.id, clientUserMessageId: "late-stop", input: [{ type: "text", text: "wait" }] };
      const { turn } = await h.call("turn/start", params);
      if (trigger === "prompt deadline") await until(() => h.bridge.sessions.get(thread.id).unresolvedTurnId);
      else await until(() => h.frames.some(frame => frame.method === "item/agentMessage/delta"));
      await assert.rejects(within(h.call("turn/interrupt", { threadId: thread.id, turnId: turn.id })), /has not confirmed/);
      const unresolved = await h.call("thread/read", { threadId: thread.id, includeTurns: true });
      assert.equal(unresolved.thread.status.type, "active");
      assert.equal(unresolved.thread.canAcceptDirectInput, false);
      assert.match(unresolved.thread.turns[0].error.message, /has not confirmed/);
      await assert.rejects(h.call("turn/start", { ...params, clientUserMessageId: "overlap" }), /has not confirmed/);
      const peer = (await h.start()).thread;
      const other = await h.call("turn/start", { threadId: peer.id, input: [{ type: "text", text: "hi" }] });
      assert.equal((await h.turnDone(other.turn.id)).status, "completed");
      await h.agents[0].request("test/release-cancel", { sessionId: thread.sessionId });
      await until(() => !h.bridge.active.has(thread.id));
      const done = await h.turnDone(turn.id);
      assert.equal(done.status, "interrupted");
      assert.equal(done.items.find(item => item.type === "agentMessage").text, "Working — stopped");
      assert.equal(h.frames.filter(frame => frame.method === "turn/completed" && frame.params.turnId === turn.id).length, 1);
      const saved = await new Sessions(join(h.root, "sessions")).initialize();
      assert.equal(saved.get(thread.id).unresolvedTurnId, undefined);
      assert.equal(saved.describe(saved.get(thread.id)).canAcceptDirectInput, true);
      assert.equal((await h.call("turn/start", params)).turn.id, turn.id);
      const next = await h.call("turn/start", { ...params, clientUserMessageId: "next", input: [{ type: "text", text: "hi" }] });
      assert.equal((await h.turnDone(next.turn.id)).status, "completed");
      assert.equal(h.agents.length, 1);
      assert.equal(h.agents[0].alive, true);
    } finally { await h.cleanup(); }
  });
}

test("Stop during ACP configuration sends no later settings or prompt", async () => {
  const h = await harness({ fault: "held-config" });
  try {
    const { thread } = await h.start();
    const { turn } = await h.call("turn/start", { threadId: thread.id, input: [{ type: "text", text: "hi" }], wonderPolicy: { planMode: true } });
    await until(async () => (await h.agents[0].request("test/state", {})).configs.length === 1);
    const stopping = h.call("turn/interrupt", { threadId: thread.id, turnId: turn.id });
    assert.equal(h.frames.some(frame => frame.method === "turn/completed" && frame.params.turnId === turn.id), false);
    await h.agents[0].request("test/release-config", {});
    assert.deepEqual((await stopping).turn, { id: turn.id, status: "interrupted" });
    assert.equal((await h.turnDone(turn.id)).status, "interrupted");
    const state = await h.agents[0].request("test/state", {});
    assert.equal(state.configs.length, 1);
    assert.equal(state.prompts.length, 0);
    assert.deepEqual((await h.agents[0].request("test/cancelled-sessions", {})).sessions, []);
  } finally { await h.cleanup(); }
});

// Neither agent's ACP has per-session access control, and a work mode is not
// one: read-only is never approximated with Plan or Cursor's Ask. Invalid
// choices must fail before a native session, persisted option, or accepted
// prompt can change.
for (const adapter of [openCodeAdapter, cursorAdapter]) {
  test(`unsupported Cupola access and approval choices are rejected before work (${adapter.label})`, async () => {
    const h = await harness({ adapter });
    const settings = new RegExp(`${adapter.label} settings`);
    try {
      for (const policy of [{ mode: "read_only" }, { mode: "read_only", planMode: true }, { mode: "full_access" }, { approvalMode: "auto" }, { approvalMode: "accept_edits" }]) {
        await assert.rejects(h.start(policy), settings);
      }
      await assert.rejects(h.start({ mode: "workspace", approvalMode: "ask", planMode: "ask" }), /Turn Plan on or off/);
      assert.equal(h.agents.length, 0);
      const { thread } = await h.start();
      for (const method of ["thread/settings/update", "thread/resume", "turn/start"]) {
        await assert.rejects(h.call(method, { threadId: thread.id, wonderPolicy: { mode: "full_access" }, input: [{ type: "text", text: "hi" }] }), settings);
        await assert.rejects(h.call(method, { threadId: thread.id, wonderPolicy: { mode: "read_only", planMode: true }, input: [{ type: "text", text: "hi" }] }), settings);
      }
      await assert.rejects(h.call("turn/start", { threadId: thread.id, wonderPolicy: { planMode: "ask" }, input: [{ type: "text", text: "hi" }] }), /Turn Plan on or off/);
      assert.equal(h.bridge.sessions.get(thread.id).turns.length, 0);
      assert.equal(h.bridge.sessions.get(thread.id).options.wonderPolicy.mode, "workspace");
      const state = await h.agents[0].request("test/state", {});
      assert.equal(state.prompts.length, 0);
      assert.equal(state.configs.length, 0);
    } finally { await h.cleanup(); }
  });
}

const permissionRequest = sessionId => ({ sessionId,
  toolCall: { toolCallId: "stale-command", kind: "execute", rawInput: { command: "ls" } },
  options: [{ optionId: "once", kind: "allow_once" }, { optionId: "reject", kind: "reject_once" }],
});

// A delayed callback can outlive its process or reply. Neither may borrow a
// newer run's authority, even when the native session ID is unchanged.
test("an approval from the previous agent cannot attach to the new running reply", async () => {
  const h = await harness();
  try {
    const { thread } = await h.start();
    const previous = h.agents[0];
    await previous.close();
    const { turn } = await h.call("turn/start", { threadId: thread.id, input: [{ type: "text", text: "wait" }] });
    await until(() => h.frames.some(frame => frame.method === "item/agentMessage/delta"));
    const outcome = await previous.onPermission(permissionRequest(thread.sessionId));
    assert.deepEqual(outcome, { outcome: "cancelled" });
    assert.equal(h.frames.some(frame => frame.method?.endsWith("/requestApproval")), false);
    await h.call("turn/interrupt", { threadId: thread.id, turnId: turn.id });
    await h.turnDone(turn.id);
  } finally { await h.cleanup(); }
});

for (const decision of ["accept", remembered("always", "allow_always"), remembered("never", "reject_always")])
for (const change of ["stopped", "finished", "replaced", "agent-exited"]) {
  test(`approval ${JSON.stringify(decision)} is cancelled when its owner is ${change} before continuation`, async () => {
    const h = await harness({ approve: null });
    try {
      const { thread } = await h.start();
      await h.call("turn/start", { threadId: thread.id, input: [{ type: "text", text: "wait" }] });
      await until(() => h.frames.some(frame => frame.method === "item/agentMessage/delta"));
      const agent = h.agents[0], run = h.bridge.active.get(thread.id);
      const request = permissionRequest(thread.sessionId);
      request.options.push({ optionId: "always", kind: "allow_always" }, { optionId: "never", kind: "reject_always" });
      const outcome = agent.onPermission(request);
      await until(() => h.frames.some(frame => frame.method?.endsWith("/requestApproval")));
      const approval = h.frames.find(frame => frame.method?.endsWith("/requestApproval"));
      // Resolve the user response, then change ownership before the awaiting
      // continuation runs. This reproduces the response/cancellation race.
      const accepted = h.bridge.receive({ id: approval.id, result: { decision } });
      if (change === "stopped") run.stopped = true;
      else if (change === "finished") run.finalizing = Promise.resolve();
      else if (change === "replaced") h.bridge.active.set(thread.id, { ...run });
      else run.exited = true;
      try { assert.deepEqual(await outcome, { outcome: "cancelled" }); }
      finally {
        run.stopped = false; run.finalizing = null; run.exited = false;
        h.bridge.active.set(thread.id, run);
        await accepted;
      }
    } finally { await h.cleanup(); }
  });
}

test("models are namespaced and unsupported actions say so", async () => {
  const h = await harness();
  try {
    const models = await h.call("model/list", {});
    assert.equal(models.data[0].id, "opencode:opencode/free-model");
    assert.equal(models.data[0].isDefault, false, "No native default was reported");
    await assert.rejects(h.call("account/rateLimits/read", {}), /does not report usage limits/);
    await assert.rejects(h.call("mcpServerStatus/list", {}), /does not report connected tool status/);
    await assert.rejects(h.call("thread/start", { cwd: h.root, model: "claude:sonnet" }), /Choose an OpenCode model/);
  } finally { await h.cleanup(); }
});

test("a refusal before a turn is recorded carries the typed retry data", async () => {
  const h = await harness();
  try {
    const thread = (await h.start()).thread;
    const id = "refused-1";
    await h.bridge.receive({ id, method: "turn/start", params: { threadId: thread.id, model: "claude:sonnet", input: [{ type: "text", text: "hi" }] } });
    const reply = h.frames.find(frame => frame.id === id && !frame.method);
    assert.equal(reply.error.code, -32000);
    assert.deepEqual(reply.error.data, { turnStart: { accepted: false, action: "retry" } });
    // Other methods stay untyped.
    await h.bridge.receive({ id: "steer-1", method: "turn/steer", params: { threadId: thread.id } });
    assert.equal(h.frames.find(frame => frame.id === "steer-1").error.data, undefined);
  } finally { await h.cleanup(); }
});

test("the configured native default leads the catalog ahead of a free model", async () => {
  const h = await harness();
  try {
    h.bridge.inspect = async () => ({ installed: true, defaultModel: "local/chosen", models: [
      { id: "opencode/free-model", name: "Free Model", providerId: "opencode", free: true },
      { id: "local/chosen", name: "Chosen Model", providerId: "local", free: false },
    ] });
    const { data } = await h.call("model/list", {});
    assert.deepEqual(data.map(model => [model.id, model.isDefault]), [
      ["opencode:local/chosen", true], ["opencode:opencode/free-model", false],
    ]);
  } finally { await h.cleanup(); }
});

async function until(check, ms = 1500) {
  const deadline = Date.now() + ms;
  while (!(await check())) {
    if (Date.now() >= deadline) throw new Error("test deadline exceeded");
    await new Promise(resolve => setTimeout(resolve, 5));
  }
}

// Session admission owns these contracts; real child pipes prove that a lost
// answer cannot admit overlapping work or cancel an independent conversation.
test("an unconfirmed prompt stays blocked across replay, Stop, and journal reload", async () => {
  const h = await harness({ fault: "ignore-cancel", promptTimeoutMs: 60, stopTimeoutMs: 60 });
  try {
    const { thread } = await h.start();
    const params = { threadId: thread.id, clientUserMessageId: "uncertain", input: [{ type: "text", text: "wait" }] };
    const { turn } = await h.call("turn/start", params);
    await until(() => h.bridge.sessions.get(thread.id).unresolvedTurnId);
    await h.bridge.sessions.flush();
    const read = await h.call("thread/read", { threadId: thread.id, includeTurns: true });
    assert.equal(read.thread.status.type, "active");
    assert.equal(read.thread.canAcceptDirectInput, false);
    assert.match(read.thread.turns[0].error.message, /has not confirmed/);
    assert.equal((await h.call("turn/start", params)).turn.id, turn.id);
    await assert.rejects(h.call("turn/interrupt", { threadId: thread.id, turnId: turn.id }), /has not confirmed/);
    await assert.rejects(h.call("turn/start", { ...params, clientUserMessageId: "fresh" }), /has not confirmed/);
    assert.equal(h.frames.some(frame => frame.method === "turn/completed" && frame.params.turnId === turn.id), false);
    const reloaded = await new Sessions(join(h.root, "sessions")).initialize();
    assert.equal((await reloaded.accept(reloaded.get(thread.id), params)).turn.id, turn.id);
    await assert.rejects(reloaded.accept(reloaded.get(thread.id), { ...params, clientUserMessageId: "after-restart" }), /has not confirmed/);
    const other = (await h.start()).thread;
    const next = await h.call("turn/start", { threadId: other.id, input: [{ type: "text", text: "hi" }] });
    assert.equal((await h.turnDone(next.turn.id)).status, "completed");
    assert.equal(h.agents.length, 1);
    assert.equal(h.agents[0].alive, true);
    // Confirming this exact process's exit is sufficient to release its turn.
    await h.agents[0].close({ graceMs: 20, killMs: 20 });
    assert.equal((await h.turnDone(turn.id)).status, "failed");
    await until(() => !h.bridge.active.has(thread.id));
    const recovered = await h.call("turn/start", { ...params, clientUserMessageId: "recovered", input: [{ type: "text", text: "hi" }] });
    assert.equal((await h.turnDone(recovered.turn.id)).status, "completed");
  } finally { await h.cleanup(); }
});

test("bridge shutdown observes the exact peer exit and saves the interrupted turn", async () => {
  const h = await harness({ fault: "ignore-close" });
  try {
    const { thread } = await h.start();
    const { turn } = await h.call("turn/start", { threadId: thread.id, input: [{ type: "text", text: "wait" }] });
    await until(() => h.frames.some(frame => frame.method === "item/agentMessage/delta"));
    const closed = await within(h.bridge.close(), 5500);
    assert.equal(h.agents[0].child.signalCode, FORCED_EXIT);
    assert.equal(closed, true);
    const saved = await new Sessions(join(h.root, "sessions")).initialize();
    assert.notEqual(saved.get(thread.id).turns.find(value => value.id === turn.id).status, "inProgress");
    assert.equal(saved.get(thread.id).unresolvedTurnId, undefined);
  } finally { await h.cleanup(); }
});

test("one durable create request produces one ACP session, including concurrent and restarted retries", async () => {
  const h = await harness();
  try {
    const requestId = randomUUID();
    const [first, retry] = await Promise.all([h.start(undefined, requestId), h.start(undefined, requestId)]);
    assert.equal(first.thread.id, retry.thread.id);
    assert.equal(first.thread.sessionId, retry.thread.sessionId);
    assert.equal((await h.agents[0].request("test/state", {})).sessions, 1);
    const stored = await new Sessions(join(h.root, "sessions")).initialize();
    const replay = await stored.create(stored.get(first.thread.id).creation.options, () => { throw new Error("duplicate remote create"); });
    assert.equal(replay.session.id, first.thread.id);
    await assert.rejects(h.start({ planMode: true }, requestId), /different settings/);
  } finally { await h.cleanup(); }
});

test("an uncertain session create is reserved before its side effect and cannot be repeated", async () => {
  const root = await mkdtemp(join(tmpdir(), "wonder-acp-create-"));
  try {
    const sessions = await new Sessions(root).initialize();
    const options = { requestId: randomUUID(), cwd: root, model: "opencode:opencode/free-model" };
    let creates = 0;
    await assert.rejects(sessions.create(options, async () => {
      creates++;
      const disk = await new Sessions(root).initialize();
      await assert.rejects(disk.create(options, () => { creates++; }), /has not confirmed/);
      throw Object.assign(new Error("timed out"), { outcomeUnknown: true });
    }), /timed out/);
    const restart = await new Sessions(root).initialize();
    // Linux and Windows hosts run OpenCode too: the advice names the computer.
    await assert.rejects(restart.create(options, () => { creates++; }), /^Error: OpenCode has not confirmed whether this conversation was created\. Check OpenCode on your computer before starting a new one\.$/);
    assert.equal(creates, 1);
  } finally { await rm(root, { recursive: true, force: true }); }
});

// Creation admission owns retries: startup cannot create a native session, but
// a pipe write (even without a receipt) may have done so. Exercise both sides
// through the existing bridge, child transport and durable journal.
for (const fault of ["missing-executable", "reject-initialize"]) {
  for (const restart of [false, true]) {
    test(`a ${fault} startup failure allows the exact create retry${restart ? " after restart" : " in process"}`, async () => {
      const h = await harness({ fault: [fault] });
      try {
        const requestId = randomUUID();
        const failed = await Promise.allSettled([h.start(undefined, requestId), h.start(undefined, requestId)]);
        assert.deepEqual(failed.map(result => result.status), ["rejected", "rejected"]);
        assert.equal(failed[0].reason.message, failed[1].reason.message);
        if (fault === "missing-executable") assert.match(failed[0].reason.message, /is not installed on this computer\.$/);
        assert.equal(h.agents.length, 1, "concurrent duplicates share the startup failure");
        const reserved = h.bridge.sessions.creations.get(requestId);
        assert.equal(reserved.acpSessionId, null);
        if (restart) await h.restart();
        await assert.rejects(h.start({ planMode: true }, requestId), /different settings/);
        const setup = h.bridge.mcpServers.bind(h.bridge);
        h.bridge.mcpServers = async (...args) => {
          const disk = await new Sessions(join(h.root, "sessions")).initialize();
          await assert.rejects(disk.create(reserved.creation.options, () => { throw new Error("retry was not reserved on disk"); }), /has not confirmed/);
          return setup(...args);
        };
        const [first, duplicate] = await Promise.all([h.start(undefined, requestId), h.start(undefined, requestId)]);
        assert.equal(first.thread.id, reserved.id, "retry keeps the original request binding");
        assert.equal(duplicate.thread.sessionId, first.thread.sessionId);
        assert.equal(h.agents.length, 2);
        assert.equal((await h.agents[1].request("test/state", {})).sessions, 1);
        const saved = await new Sessions(join(h.root, "sessions")).initialize();
        assert.equal(saved.creations.get(requestId).acpSessionId, first.thread.sessionId);
        await h.restart();
        assert.equal((await h.start(undefined, requestId)).thread.sessionId, first.thread.sessionId);
        assert.equal(h.agents.length, 2, "replaying a saved success does not start another peer");
        assert.equal(h.frames.filter(frame => frame.method === "thread/started").length, 1);
      } finally { await h.cleanup(); }
    });
  }
}

test("a peer exiting during MCP setup can retry because session/new was never written", async () => {
  const h = await harness();
  try {
    const setup = h.bridge.mcpServers.bind(h.bridge);
    h.bridge.mcpServers = async (session, agent) => {
      const servers = await setup(session, agent);
      assert.equal((await agent.request("test/state", {})).sessions, 0);
      await agent.close();
      return servers;
    };
    const requestId = randomUUID();
    await assert.rejects(h.start(undefined, requestId), /not running/);
    h.bridge.mcpServers = setup;
    const { thread } = await h.start(undefined, requestId);
    assert.equal((await h.agents[1].request("test/state", {})).sessions, 1);
    assert.equal((await new Sessions(join(h.root, "sessions")).initialize()).creations.get(requestId).acpSessionId, thread.sessionId);
  } finally { await h.cleanup(); }
});

for (const fault of ["lost-create", "reject-create"]) {
  for (const recovering of [false, true]) {
    test(`a ${fault} native create stays uncertain in process and after restart${recovering ? " on a startup retry" : ""}`, async () => {
      const h = await harness({ fault: recovering ? ["missing-executable", fault] : fault });
      try {
        const requestId = randomUUID();
        if (recovering) await assert.rejects(h.start(undefined, requestId), /not installed/);
        const agent = await h.bridge.agent();
        const request = agent.request.bind(agent);
        agent.request = (method, params, options) => request(method, params, method === "session/new" ? { ...options, timeoutMs: 50 } : options);
        const failed = await Promise.allSettled([h.start(undefined, requestId), h.start(undefined, requestId)]);
        assert.deepEqual(failed.map(result => result.status), ["rejected", "rejected"]);
        assert.equal(failed[0].reason.message, failed[1].reason.message);
        // The provider's rejection deliberately uses the same words as the
        // retryable initialization failure. Text never establishes admission.
        assert.match(failed[0].reason.message, fault === "lost-create" ? /did not confirm/ : /Synthetic startup refused/);
        assert.equal((await agent.request("test/state", {})).sessions, 1);
        await assert.rejects(h.start(undefined, requestId), /has not confirmed/);
        assert.equal((await agent.request("test/state", {})).sessions, 1);
        const peers = h.agents.length;
        await h.restart();
        await assert.rejects(h.start(undefined, requestId), /has not confirmed/);
        assert.equal(h.agents.length, peers, "uncertain creation never starts a replacement peer");
      } finally { await h.cleanup(); }
    });
  }
}

test("a failed reservation write admits no native creation and concurrent duplicates share its failure", async () => {
  const h = await harness();
  try {
    const sessions = h.bridge.sessions;
    const save = sessions.save.bind(sessions);
    const failure = new Error("Synthetic reservation write failure");
    let release;
    const gate = new Promise(resolve => { release = resolve; });
    sessions.save = async () => { await gate; throw failure; };
    const requestId = randomUUID();
    const first = h.start(undefined, requestId);
    await until(() => sessions.creating.has(requestId));
    const reserved = sessions.creations.get(requestId);
    const duplicate = sessions.create(reserved.creation.options, () => { throw new Error("duplicate admitted native work"); });
    const outcomes = Promise.allSettled([first, duplicate]);
    release();
    const failed = await outcomes;
    assert.deepEqual(failed.map(result => result.status), ["rejected", "rejected"]);
    assert.deepEqual(failed.map(result => result.reason.message), [failure.message, failure.message]);
    assert.equal(h.agents.length, 0);
    sessions.save = save;
    const { thread } = await h.start(undefined, requestId);
    assert.equal((await h.agents[0].request("test/state", {})).sessions, 1);
    assert.equal((await new Sessions(join(h.root, "sessions")).initialize()).creations.get(requestId).acpSessionId, thread.sessionId);
  } finally { await h.cleanup(); }
});

test("a rejected exit from Plan never sends a prompt", async () => {
  const h = await harness({ fault: "reject-build" });
  try {
    const { thread } = await h.start();
    const input = [{ type: "text", text: "hi" }];
    const planned = await h.call("turn/start", { threadId: thread.id, input, wonderPolicy: { planMode: true } });
    assert.equal((await h.turnDone(planned.turn.id)).status, "completed");
    const building = await h.call("turn/start", { threadId: thread.id, input, wonderPolicy: { planMode: false } });
    const rejected = await h.turnDone(building.turn.id);
    assert.equal(rejected.status, "failed");
    const state = await h.agents[0].request("test/state", {});
    assert.equal(state.prompts.length, 1);
    assert.equal(state.prompts[0].mode, "plan");
    assert.equal(state.configs[0].configId, "model");
    const config = h.bridge.sessions.get(thread.id).configuration;
    assert.equal(config.configOptions.find(option => option.category === "mode").currentValue, "plan");
  } finally { await h.cleanup(); }
});

for (const fault of ["unconfirmed-config", "legacy-modes", "custom-config"]) {
  test(`configuration handles ${fault} without guessing the active mode`, async () => {
    const h = await harness({ fault });
    try {
      const { thread } = await h.start();
      const { turn } = await h.call("turn/start", { threadId: thread.id, input: [{ type: "text", text: "hi" }], wonderPolicy: { planMode: true } });
      assert.equal((await h.turnDone(turn.id)).status, fault === "unconfirmed-config" ? "failed" : "completed");
      const state = await h.agents[0].request("test/state", {});
      assert.equal(state.prompts.length, fault === "unconfirmed-config" ? 0 : 1);
      if (fault !== "unconfirmed-config") assert.equal(state.prompts[0].mode, "plan");
      if (fault === "custom-config") assert.deepEqual(state.configs.map(config => config.configId), ["provider-model", "agent-mode"]);
    } finally { await h.cleanup(); }
  });
}


test("a lost transport cannot report idle until its owned process exit is observed", async () => {
  const h = await harness({ fault: "lost-transport" });
  try {
    const { thread } = await h.start();
    const { turn } = await h.call("turn/start", { threadId: thread.id, input: [{ type: "text", text: "hi" }] });
    await until(() => h.bridge.sessions.get(thread.id).unresolvedTurnId);
    assert.equal(h.agents[0].alive, false);
    assert.equal(h.agents[0].child.exitCode, null);
    assert.equal(h.agents[0].child.signalCode, null);
    await assert.rejects(h.call("turn/start", { threadId: thread.id, input: [{ type: "text", text: "overlap" }] }), /has not confirmed/);
    assert.equal((await h.turnDone(turn.id)).status, "failed");
    assert.equal(h.agents[0].child.signalCode, FORCED_EXIT);
  } finally { await h.cleanup(); }
});

test("an unconfirmed child close returns failure and preserves unresolved admission", async () => {
  const h = await harness({ fault: "ignore-cancel", promptTimeoutMs: 60, stopTimeoutMs: 60 });
  let realClose;
  try {
    const { thread } = await h.start();
    const { turn } = await h.call("turn/start", { threadId: thread.id, input: [{ type: "text", text: "wait" }] });
    await until(() => h.bridge.sessions.get(thread.id).unresolvedTurnId);
    const agent = h.agents[0];
    realClose = agent.close.bind(agent);
    // Fault injection only at the OS exit observation boundary: the real child
    // remains alive so a false return cannot be mistaken for process death.
    agent.close = async () => false;
    assert.equal(await h.bridge.close(), false);
    const saved = await new Sessions(join(h.root, "sessions")).initialize();
    assert.equal(saved.get(thread.id).unresolvedTurnId, turn.id);
    assert.equal(saved.describe(saved.get(thread.id)).status.type, "active");
  } finally {
    if (realClose) assert.equal(await realClose({ graceMs: 20, killMs: 20 }), true);
    // The injected failed close is cached. Actual process exit starts a later
    // session save; let that finish before removing the fixture's directory.
    await within(Promise.all([...h.bridge.active.values()].map(run => run.finalizing ?? run.finished)));
    await h.cleanup();
  }
});


test("an idle configuration change is applied before the next prompt", async () => {
  const h = await harness();
  try {
    const { thread } = await h.start();
    const input = [{ type: "text", text: "hi" }];
    const first = await h.call("turn/start", { threadId: thread.id, input });
    await h.turnDone(first.turn.id);
    await until(() => !h.bridge.active.has(thread.id));
    await h.agents[0].request("test/change-mode", { sessionId: thread.sessionId, mode: "plan" });
    const next = await h.call("turn/start", { threadId: thread.id, input });
    assert.equal((await h.turnDone(next.turn.id)).status, "completed");
    const state = await h.agents[0].request("test/state", {});
    assert.deepEqual(state.prompts.map(prompt => prompt.mode), ["build", "build"]);
    assert.ok(state.configs.some(config => config.configId === "mode" && config.value === "build"));
  } finally { await h.cleanup(); }
});

test("a prompt reply without a valid stop reason does not infer completion", async () => {
  const h = await harness({ fault: "invalid-completion" });
  try {
    const { thread } = await h.start();
    const { turn } = await h.call("turn/start", { threadId: thread.id, input: [{ type: "text", text: "hi" }] });
    await until(() => h.bridge.sessions.get(thread.id).unresolvedTurnId);
    assert.equal(h.frames.some(frame => frame.method === "turn/completed" && frame.params.turnId === turn.id), false);
    await assert.rejects(within(h.call("turn/interrupt", { threadId: thread.id, turnId: turn.id })), /has not confirmed/);
    await assert.rejects(h.call("turn/start", { threadId: thread.id, input: [{ type: "text", text: "again" }] }), /has not confirmed/);
  } finally { await h.cleanup(); }
});


test("shutdown waits for a replaced process that is still stopping", async () => {
  const h = await harness({ fault: ["lost-transport", undefined] });
  try {
    const { thread } = await h.start();
    await h.call("turn/start", { threadId: thread.id, input: [{ type: "text", text: "hi" }] });
    await until(() => h.bridge.sessions.get(thread.id).unresolvedTurnId);
    await h.start();
    assert.equal(h.agents.length, 2);
    assert.equal(h.agents[0].child.signalCode, null);
    assert.equal(await within(h.bridge.close(), 5500), true);
    assert.equal(h.agents[0].child.signalCode, FORCED_EXIT);
    assert.ok(h.agents[1].child.exitCode !== null || h.agents[1].child.signalCode !== null);
  } finally { await h.cleanup(); }
});

// Fail the real atomic rename, keeping the last valid journal for recovery.
// The writer gates below control overlap only; neither save nor accept is mocked.
async function blockJournal(sessions, session) {
  const path = join(sessions.root, `${session.id}.json`), backup = `${path}.before-failure`;
  await rename(path, backup);
  await mkdir(path);
  let restored = false;
  return async () => {
    if (restored) return;
    await rm(path, { recursive: true });
    await rename(backup, path);
    restored = true;
  };
}

test("a journal save waits out a reader holding the file open", async () => {
  // Windows refuses to rename onto an open file (EPERM); a reader or virus
  // scanner holding the last save must not fail the turn's journal write.
  const h = await harness();
  try {
    const { thread } = await h.start();
    const sessions = h.bridge.sessions, session = sessions.get(thread.id);
    const path = join(sessions.root, `${session.id}.json`);
    const reader = await open(path, "r");
    const released = new Promise(resolve => setTimeout(resolve, 150)).then(() => reader.close());
    session.updatedAt += 1;
    await sessions.save(session);
    await released;
    assert.equal(JSON.parse(await readFile(path, "utf8")).updatedAt, session.updatedAt);
  } finally { await h.cleanup(); }
});

test("a failed turn journal rejects overlapping deliveries before execution and retries exactly once", async () => {
  const h = await harness();
  const gate = Promise.withResolvers();
  let recover = async () => {}, outcomes;
  try {
    const { thread } = await h.start();
    const sessions = h.bridge.sessions, session = sessions.get(thread.id);
    const path = join(sessions.root, `${session.id}.json`), before = await readFile(path, "utf8");
    recover = await blockJournal(sessions, session);
    sessions.writes.set(session.id, gate.promise);
    const params = { threadId: thread.id, clientUserMessageId: "retryable", input: [{ type: "text", text: "hi" }] };
    outcomes = Promise.allSettled(Array.from({ length: 3 }, () => h.call("turn/start", params)));
    await new Promise(resolve => setImmediate(resolve));
    assert.equal(h.bridge.active.size, 0);
    assert.equal((await h.agents[0].request("test/state", {})).prompts.length, 0);
    gate.resolve();
    const rejected = await outcomes;
    assert.deepEqual(rejected.map(result => result.status), ["rejected", "rejected", "rejected"]);
    for (const result of rejected) assert.match(result.reason.message, new RegExp(JOURNAL_BLOCKED));
    assert.equal(h.bridge.active.size, 0);
    assert.equal(h.frames.some(frame => frame.method === "turn/started"), false);
    assert.equal((await h.agents[0].request("test/state", {})).prompts.length, 0);
    assert.equal(sessions.accepting.size, 0, "a rejected admission must release every pending receipt");
    assert.deepEqual(session.turns, []);
    assert.deepEqual(session.receipts, {});
    assert.equal(sessions.describe(session).canAcceptDirectInput, true);
    await recover();
    assert.equal(await readFile(path, "utf8"), before);
    const [retried, duplicate] = await Promise.all([h.call("turn/start", params), h.call("turn/start", params)]);
    assert.equal(retried.turn.id, duplicate.turn.id);
    assert.equal((await h.turnDone(retried.turn.id)).status, "completed");
    await until(() => !h.bridge.active.has(thread.id));
    assert.equal((await h.agents[0].request("test/state", {})).prompts.length, 1);
    const saved = (await new Sessions(sessions.root).initialize()).get(thread.id);
    assert.equal(saved.turns.length, 1);
    assert.equal(saved.turns[0].id, retried.turn.id);
    assert.deepEqual(saved.receipts, { retryable: { id: retried.turn.id, status: "completed" } });
    await h.restart();
    assert.equal((await h.call("turn/start", params)).turn.id, retried.turn.id);
    assert.equal(h.agents.length, 1, "replaying a reloaded receipt starts no executor or peer");
    assert.equal(h.bridge.active.size, 0);
  } finally {
    gate.resolve();
    await recover();
    await outcomes;
    await h.cleanup();
  }
});

for (const queuedUpdate of [false, true]) {
  test(`failed turn admission preserves the retention boundary${queuedUpdate ? " and concurrent terminal saves" : " and timestamp"}`, async () => {
    const root = await mkdtemp(join(tmpdir(), "wonder-acp-rollback-"));
    const gate = Promise.withResolvers(), recovered = Promise.withResolvers();
    let recover = async () => {}, outcomes, later;
    try {
      const sessions = await new Sessions(root).initialize();
      const { session } = await sessions.create({ requestId: randomUUID(), cwd: root }, async () => ({ sessionId: "saved" }));
      session.turns = Array.from({ length: 200 }, (_, n) => ({ id: randomUUID(), clientUserMessageId: `old-${n}`,
        status: "completed", items: [], error: null }));
      session.receipts.retired = { id: randomUUID(), status: "completed" };
      session.updatedAt = 17;
      await sessions.save(session);
      const oldest = session.turns[0], earlier = session.turns.at(-1);
      const receipts = structuredClone(session.receipts);
      recover = await blockJournal(sessions, session);
      sessions.writes.set(session.id, gate.promise);
      const params = { clientUserMessageId: "failed", input: [{ type: "text", text: "hi" }] };
      outcomes = Promise.allSettled([sessions.accept(session, params), sessions.accept(session, params)]);
      if (queuedUpdate) {
        // Capture this save while the failed turn is still speculative, then
        // let its real writer run only after the failed admission settles.
        const failedWrite = sessions.writes.get(session.id);
        sessions.writes.set(session.id, failedWrite.catch(() => {}).then(() => recovered.promise));
        earlier.items.push({ id: "earlier-result", type: "agentMessage", text: "Earlier response finished" });
        session.configuration = { currentModeId: "build" };
        session.updatedAt = 91;
        later = sessions.save(session);
      }
      gate.resolve();
      const rejected = await outcomes;
      assert.deepEqual(rejected.map(result => result.status), ["rejected", "rejected"]);
      for (const result of rejected) assert.equal(result.reason.code, JOURNAL_BLOCKED);
      assert.equal(rejected[0].reason, rejected[1].reason);
      assert.equal(sessions.accepting.size, 0);
      assert.equal(session.turns.length, 200);
      assert.equal(session.turns[0], oldest, "failure cannot evict a previously accepted turn");
      assert.equal(session.turns.at(-1), earlier, "rollback must preserve the running bridge's references");
      assert.deepEqual(session.receipts, receipts);
      assert.equal(session.updatedAt, queuedUpdate ? 91 : 17);
      await recover();
      recovered.resolve();
      await later;
      const stored = (await new Sessions(root).initialize()).get(session.id);
      assert.equal(stored.turns.length, 200);
      assert.equal(stored.turns[0].id, oldest.id);
      assert.deepEqual(stored.receipts, receipts, "a queued save must not resurrect the failed receipt");
      assert.equal(stored.turns.some(turn => turn.clientUserMessageId === "failed"), false);
      if (queuedUpdate) {
        assert.deepEqual(stored.turns.at(-1), earlier);
        assert.deepEqual(stored.configuration, session.configuration);
        assert.equal(stored.updatedAt, 91);
      }
      const { turn } = await sessions.accept(session, params);
      turn.status = "completed";
      await sessions.save(session);
      const reloaded = await new Sessions(root).initialize(), saved = reloaded.get(session.id);
      assert.equal(saved.turns.length, 200);
      assert.equal(saved.turns[0].clientUserMessageId, "old-1");
      assert.equal(saved.turns.filter(value => value.clientUserMessageId === "failed").length, 1);
      assert.deepEqual(saved.receipts.failed, { id: turn.id, status: "completed" });
      assert.deepEqual(saved.receipts.retired, receipts.retired);
      assert.equal((await reloaded.accept(saved, params)).turn.id, turn.id);
      assert.equal((await reloaded.accept(saved, { clientUserMessageId: oldest.clientUserMessageId })).turn.id, oldest.id);
    } finally {
      gate.resolve();
      await recover();
      recovered.resolve();
      await Promise.allSettled([outcomes, later]);
      await rm(root, { recursive: true, force: true });
    }
  });
}

// Every save rewrites the whole journal, so receipts are bounded beside the
// 200-turn window: live-turn receipts and a recent tail of evicted ones stay.
test("saves and reloads keep receipts bounded while retries inside the window deduplicate", async () => {
  const root = await mkdtemp(join(tmpdir(), "wonder-acp-receipt-bound-"));
  try {
    const sessions = await new Sessions(root).initialize();
    const { session } = await sessions.create({ requestId: randomUUID(), cwd: root }, async () => ({ sessionId: "saved" }));
    session.turns = Array.from({ length: 200 }, (_, n) => ({ id: randomUUID(), clientUserMessageId: `old-${n}`,
      status: "completed", items: [], error: null }));
    for (let n = 0; n < 5000; n++) session.receipts[`legacy-${n}`] = { id: randomUUID(), status: "completed" };
    await sessions.save(session);
    const path = join(root, `${session.id}.json`);
    const stored = JSON.parse(await readFile(path, "utf8"));
    assert.equal(Object.keys(stored.receipts).length, RECEIPT_LIMIT);
    for (const turn of session.turns) assert.equal(stored.receipts[turn.clientUserMessageId].id, turn.id, "live turns keep their receipts");
    assert.ok(stored.receipts["legacy-4999"] && !stored.receipts["legacy-0"]);
    const receiptBytes = JSON.stringify(stored.receipts).length;

    for (let n = 0; n < 5; n++) {
      const { turn } = await sessions.accept(session, { clientUserMessageId: `new-${n}`, input: [] });
      turn.status = "completed";
    }
    await sessions.save(session);
    const grown = JSON.parse(await readFile(path, "utf8")).receipts;
    assert.equal(Object.keys(grown).length, RECEIPT_LIMIT);
    assert.ok(JSON.stringify(grown).length < receiptBytes * 1.05, "receipt bytes stay flat as messages are added");

    assert.equal((await sessions.accept(session, { clientUserMessageId: "legacy-4999" })).duplicate, true);
    assert.equal((await sessions.accept(session, { clientUserMessageId: "legacy-0", input: [] })).duplicate, false);

    // An oversized journal from an older build is trimmed on load.
    const journal = JSON.parse(await readFile(path, "utf8"));
    for (let n = 0; n < 3000; n++) journal.receipts[`older-${n}`] = { id: randomUUID(), status: "completed" };
    await writeFile(path, JSON.stringify(journal));
    const reloaded = (await new Sessions(root).initialize()).get(session.id);
    assert.equal(Object.keys(reloaded.receipts).length, RECEIPT_LIMIT);
    for (const turn of reloaded.turns) assert.equal(reloaded.receipts[turn.clientUserMessageId].id, turn.id);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test("a duplicate receipt waits for its first durable acceptance", async () => {
  const root = await mkdtemp(join(tmpdir(), "wonder-acp-receipt-"));
  try {
    const sessions = await new Sessions(root).initialize();
    const { session } = await sessions.create({ requestId: randomUUID(), cwd: root }, async () => ({ sessionId: "saved" }));
    const save = sessions.save.bind(sessions);
    let release;
    const gate = new Promise(resolve => { release = resolve; });
    sessions.save = async value => { await gate; return save(value); };
    const params = { clientUserMessageId: "same", input: [{ type: "text", text: "hi" }] };
    let replayed = false;
    const first = sessions.accept(session, params);
    const retry = sessions.accept(session, params).then(result => { replayed = true; return result; });
    await new Promise(resolve => setImmediate(resolve));
    assert.equal(replayed, false);
    release();
    const [accepted, duplicate] = await Promise.all([first, retry]);
    assert.equal(accepted.turn.id, duplicate.turn.id);
    const saved = await new Sessions(root).initialize();
    assert.equal(saved.get(session.id).turns[0].id, accepted.turn.id);
  } finally { await rm(root, { recursive: true, force: true }); }
});

// ACP kind/locations are authoritative; changing a title must not change semantics.
test("native tool kinds and updated targets reach the live activity", async () => {
  const { TurnProjection } = await import("../projection.mjs");
  const events = [];
  const p = new TurnProjection({ threadId: "t", turnId: "turn", emit: e => events.push(e) }); p.start();
  p.update({ sessionUpdate: "tool_call", toolCallId: "r", kind: "read", title: "read", status: "in_progress", rawInput: {} });
  p.update({ sessionUpdate: "tool_call_update", toolCallId: "r", title: "a.swift", locations: [{ path: "/p/a.swift", line: 1 }] });
  const live = events.filter(e => e.method === "item/started").at(-1).params.item;
  assert.deepEqual(live.toolAction, { kind: "read", paths: ["/p/a.swift"] });
  p.update({ sessionUpdate: "tool_call_update", toolCallId: "r", status: "failed" });
  assert.equal(events.at(-1).params.item.status, "failed");
  assert.deepEqual(events.at(-1).params.item.toolAction, live.toolAction);
  const before = events.length;
  p.update({ sessionUpdate: "tool_call_update", toolCallId: "r", status: "in_progress" });
  assert.equal(events.length, before, "Late running metadata must not revive a failed action");
  p.update({ sessionUpdate: "tool_call", toolCallId: "u", kind: "other", title: "read all secrets", rawInput: { filePath: "/private" } });
  assert.equal(events.at(-1).params.item.toolAction, undefined);
});

// Turn termination must not complete unfinished native plan entries.
test("ACP checklists preserve native states and update the same item", async () => {
  const { TurnProjection } = await import("../projection.mjs");
  const events = [];
  const p = new TurnProjection({ threadId: "thread", turnId: "turn", emit: e => events.push(e), now: () => 1000 });
  p.start();
  for (const status of ["pending", "in_progress", "future_status"]) {
    p.update({ sessionUpdate: "plan", entries: [{ content: "Review output", status }] });
  }
  const result = p.finish({ stopReason: "end_turn" });
  assert.equal(result.items.length, 1);
  assert.equal(result.items[0].type, "taskChecklist");
  assert.equal(result.items[0].taskChecklist.completed, 0);
  assert.equal(result.items[0].taskChecklist.entries[0].status, "future_status");
  assert.deepEqual(events.filter(e => e.method === "item/completed").map(e => e.params.item.updatedAt), ["1000", "1001", "1002"]);
  assert.deepEqual(events.filter(e => e.method === "item/completed").map(e => e.params.notificationId),
    ["turn:tasks:1000", "turn:tasks:1001", "turn:tasks:1002"], "Distinct updates must survive the daemon's completed-item deduplication");
});


// Catalogs are optional native session state. Exercise the existing real-pipe
// lifecycle owner: no OpenCode command inventory, CLI or provider process.
const commandRows = [
  { name: "explain", description: "Explain a change", input: { hint: "topic" }, _meta: { source: "synthetic" } },
  { name: "explain", description: "Another native row with the same name", input: { hint: "other topic" } },
];
const commands = (h, thread) => h.call("thread/commands/list", { threadId: thread.id });

for (const timing of ["before", "after"]) {
  test(`native commands sent ${timing} session/new are retained without discovery work`, async () => {
    const h = await harness({ commandScript: [{ [timing]: [{ commands: commandRows }] }] });
    try {
      const { thread } = await h.start();
      const peer = h.agents[0];
      const before = await peer.request("test/state", {});
      const timer = h.bridge.agentEntry.timer;
      h.bridge.createAgent = () => { throw new Error("read started a native peer"); };
      h.bridge.inspect = () => { throw new Error("read inspected a provider"); };
      const request = peer.request;
      peer.request = () => { throw new Error("read made a native request"); };
      const read = await commands(h, thread);
      assert.equal(read.availability, "ready");
      assert.equal(read.sessionId, thread.sessionId);
      assert.deepEqual(read.commands, commandRows);
      assert.ok(read.revision > 0);
      assert.equal(typeof read.peerId, "string");
      assert.deepEqual(await commands(h, thread), read);
      assert.equal(h.bridge.agentEntry.timer, timer, "reads do not extend idle lifetime");
      peer.request = request;
      assert.deepEqual(await peer.request("test/state", {}), before, "reads issue no native request");
    } finally { await h.cleanup(); }
  });
}


test("running and idle catalogs fully replace native rows, preserve empty, and never enter history", async () => {
  const h = await harness();
  try {
    const { thread } = await h.start();
    const peer = h.agents[0];
    assert.equal((await commands(h, thread)).availability, "unknown", "omitted native advertising is not a known-empty catalog");
    assert.equal((await commands(h, thread)).reason, null);
    const { turn } = await h.call("turn/start", { threadId: thread.id, input: [{ type: "text", text: "wait" }] });
    await until(() => h.frames.some(f => f.method === "item/agentMessage/delta"));
    const framesBefore = h.frames.filter(f => f.method).length;
    await peer.request("test/commands", { sessionId: thread.sessionId, commands: commandRows });
    const active = await commands(h, thread);
    assert.deepEqual(active.commands, commandRows);
    const replacement = [{ name: "different", description: "Entirely replaced", input: null }];
    await peer.request("test/commands", { sessionId: thread.sessionId, commands: replacement });
    const replaced = await commands(h, thread);
    assert.deepEqual(replaced.commands, replacement);
    assert.ok(replaced.revision > active.revision);
    assert.equal(h.frames.filter(f => f.method).length, framesBefore, "capability updates emit no turn items");
    await h.call("turn/interrupt", { threadId: thread.id, turnId: turn.id });
    await until(() => !h.bridge.active.has(thread.id));
    await peer.request("test/commands", { sessionId: thread.sessionId, commands: [] });
    const empty = await commands(h, thread);
    assert.equal(empty.availability, "ready");
    assert.deepEqual(empty.commands, []);
    assert.ok(empty.revision > replaced.revision);
    const reopened = await new Sessions(join(h.root, "sessions")).initialize();
    const cold = new AcpBridge({ sessions: reopened, send: async () => {},
      createAgent: () => { throw new Error("cold read started a peer"); }, inspect: () => { throw new Error("cold read inspected a provider"); } });
    assert.equal((await cold.request("thread/commands/list", { threadId: thread.id })).availability, "unavailable");
    const history = await h.call("thread/items/list", { threadId: thread.id });
    assert.deepEqual(history.data.map(row => row.item.type), ["userMessage", "agentMessage"]);
    assert.equal(JSON.stringify(reopened.get(thread.id)).includes("Entirely replaced"), false, "catalog is not journaled");
  } finally { await h.cleanup(); }
});

for (const method of ["resume", "load"]) {
  for (const timing of ["before", "after"]) {
    test(`commands sent ${timing} session/${method} require successful exact-session attachment`, async () => {
      const h = await harness({ fault: method === "load" ? "load-only" : undefined,
        commandScript: [{ [timing]: [{ commands: commandRows }], hold: true }] });
      try {
        // A saved cold session has no owned native peer. A normal accepted send
        // is the sole operation that creates and attaches the synthetic peer.
        const { session } = await h.bridge.sessions.create({ requestId: randomUUID(), cwd: h.root,
          model: "opencode:opencode/free-model" }, async () => ({ sessionId: "saved-native" }));
        const thread = { id: session.id, sessionId: session.acpSessionId };
        assert.equal((await commands(h, thread)).availability, "unavailable");
        assert.equal(h.agents.length, 0);
        const { turn } = await h.call("turn/start", { threadId: thread.id, input: [{ type: "text", text: "hi" }] });
        await until(async () => h.agents[0]?.alive && (await h.agents[0].request("test/state", {})).setups.includes(thread.sessionId));
        const pending = await commands(h, thread);
        assert.equal(pending.availability, "unavailable", "setup notification cannot make an attachment ready");
        await h.agents[0].request("test/release-setup", { sessionId: thread.sessionId });
        await h.turnDone(turn.id);
        const read = await commands(h, thread);
        assert.equal(read.availability, "ready");
        assert.deepEqual(read.commands, commandRows);
        const state = await h.agents[0].request("test/state", {});
        assert.equal(state.controls.filter(c => c.method === `session/${method}`).length, 1);
        assert.equal(state.controls.some(c => c.method === "session/new"), false);
        assert.equal(session.turns[0].items.filter(i => i.type === "agentMessage").length, 1);
      } finally { await h.cleanup(); }
    });
  }
}

test("concurrent new-session catalogs bind by receipt ID and discard unmatched setup updates", async () => {
  const a = [{ name: "first", description: "First session" }], b = [{ name: "second", description: "Second session" }];
  const h = await harness({ commandScript: [
    { sessionId: "first-native", before: [{ commands: a }, { sessionId: "unmatched-native", commands: commandRows }], hold: true },
    { sessionId: "second-native", before: [{ commands: b }], hold: true },
    { sessionId: "unmatched-native" },
  ] });
  try {
    const starts = [h.start(), h.start()];
    await until(async () => h.agents[0]?.alive && (await h.agents[0].request("test/state", {})).setups.length === 2);
    for (const session of h.bridge.sessions.values.values()) assert.equal((await commands(h, session)).availability, "unavailable");
    await h.agents[0].request("test/release-setup", { sessionId: "second-native" });
    await h.agents[0].request("test/release-setup", { sessionId: "first-native" });
    const threads = (await Promise.all(starts)).map(value => value.thread);
    for (const thread of threads) assert.deepEqual((await commands(h, thread)).commands, thread.sessionId === "first-native" ? a : b);
    const { thread } = await h.start();
    assert.equal((await commands(h, thread)).availability, "unknown", "a later create cannot claim an old unmatched notification");
  } finally { await h.cleanup(); }
});

for (const setup of ["new", "resume", "load"]) {
  test(`failed session/${setup} discards setup commands, including notifications after timeout`, async () => {
    const h = await harness({ fault: setup === "load" ? "load-only" : undefined, commandScript: [
      { sessionId: "failed-native", before: [{ commands: commandRows }], after: [{ commands: commandRows }], hold: true },
      { sessionId: "failed-native" },
    ] });
    try {
      const peer = await h.bridge.agent();
      const nativeRequest = peer.request.bind(peer);
      peer.request = (method, params, options) => nativeRequest(method, params,
        method === `session/${setup}` ? { ...options, timeoutMs: 40 } : options);
      if (setup === "new") {
        await assert.rejects(h.start(), /confirm/);
      } else {
        const { session } = await h.bridge.sessions.create({ requestId: randomUUID(), cwd: h.root }, async () => ({ sessionId: "failed-native" }));
        await assert.rejects(h.bridge.attach(peer, { session }), /confirm/);
        assert.equal((await commands(h, session)).availability, "unavailable");
      }
      await peer.request("test/release-setup", { sessionId: "failed-native" });
      const { thread } = await h.start();
      assert.equal((await commands(h, thread)).availability, "unknown", "failed or late setup rows cannot survive a new attachment");
    } finally { await h.cleanup(); }
  });
}

test("invalid native replacements fail closed and the next complete update recovers", async () => {
  const h = await harness();
  try {
    const { thread } = await h.start();
    const peer = h.agents[0];
    const invalid = [null, {}, [{ name: "missing-description" }], [{ name: "bad-hint", description: "", input: {} }],
      [{ name: "x".repeat(257), description: "" }], [{ name: "large-description", description: "x".repeat(16_385) }],
      Array.from({ length: 513 }, () => ({ name: "row", description: "" })),
      [{ name: "large-metadata", description: "", _meta: { value: "x".repeat(256 * 1024) } }]];
    for (const rows of invalid) {
      await peer.request("test/commands", { sessionId: thread.sessionId, commands: commandRows });
      const before = await commands(h, thread);
      await peer.request("test/commands", { sessionId: thread.sessionId, commands: rows });
      const rejected = await commands(h, thread);
      assert.equal(rejected.availability, "unknown");
      assert.equal(rejected.reason, "invalidCatalog");
      assert.equal(rejected.commands, null);
      assert.ok(rejected.revision > before.revision);
    }
    await peer.request("test/commands", { sessionId: thread.sessionId, commands: [] });
    assert.deepEqual((await commands(h, thread)).commands, []);
  } finally { await h.cleanup(); }
});

test("unknown-ID setup floods discard evicted catalogs and cannot publish another session's rows", async () => {
  const h = await harness({ commandScript: [{ before: [
    { commands: commandRows },
    ...Array.from({ length: 33 }, (_, i) => ({ sessionId: `foreign-${i}`, commands: commandRows })),
  ] }] });
  try {
    const { thread } = await h.start();
    const read = await commands(h, thread);
    assert.equal(read.availability, "unknown");
    assert.equal(read.commands, null);
    await h.agents[0].request("test/commands", { sessionId: thread.sessionId, commands: [] });
    assert.deepEqual((await commands(h, thread)).commands, []);
  } finally { await h.cleanup(); }
});

test("catalog retention is bounded without presenting evicted rows as known-empty", async () => {
  const h = await harness({ commandScript: Array.from({ length: 65 }, () => ({ before: [{ commands: commandRows }] })) });
  try {
    const threads = [];
    for (let i = 0; i < 65; i++) threads.push((await h.start()).thread);
    assert.equal((await commands(h, threads[0])).availability, "unknown");
    assert.deepEqual((await commands(h, threads.at(-1))).commands, commandRows);
    await h.agents[0].request("test/commands", { sessionId: threads[0].sessionId, commands: [] });
    assert.deepEqual((await commands(h, threads[0])).commands, []);
  } finally { await h.cleanup(); }
});

test("catalog reads do not keep an idle peer alive or revive catalogs after expiry", async () => {
  const h = await harness({ idleMs: 80, commandScript: [{ before: [{ commands: commandRows }] }] });
  try {
    const { thread } = await h.start();
    const live = await commands(h, thread);
    assert.equal(live.availability, "ready");
    h.bridge.createAgent = () => { throw new Error("cold read revived a peer"); };
    h.bridge.inspect = () => { throw new Error("cold read inspected a provider"); };
    await until(async () => (await commands(h, thread)).availability === "unavailable");
    assert.equal(h.agents.length, 1);
    assert.equal(h.agents[0].alive, false);
    assert.equal((await commands(h, thread)).commands, null);
  } finally { await h.cleanup(); }
});

test("replaced peers cannot change an idle or active session catalog", async () => {
  const h = await harness({ commandScript: [{ before: [{ commands: commandRows }] }] });
  try {
    const { thread } = await h.start();
    const old = h.agents[0];
    const original = await commands(h, thread);
    await old.close();
    assert.equal((await commands(h, thread)).availability, "unavailable");
    const { turn } = await h.call("turn/start", { threadId: thread.id, input: [{ type: "text", text: "wait" }] });
    await until(() => h.frames.some(f => f.method === "item/agentMessage/delta"));
    const current = await commands(h, thread);
    assert.notEqual(current.peerId, original.peerId);
    assert.equal(current.availability, "ready");
    const stale = () => h.bridge.update({ sessionId: thread.sessionId, update: { sessionUpdate: "available_commands_update", availableCommands: [] } }, old);
    // Model a queued callback from a previous peer independently of its alive
    // flag, so the exact-object fence (not merely closed state) is exercised.
    old.alive = true;
    stale();
    assert.deepEqual(await commands(h, thread), current);
    await h.call("turn/interrupt", { threadId: thread.id, turnId: turn.id });
    await until(() => !h.bridge.active.has(thread.id));
    stale();
    assert.deepEqual(await commands(h, thread), current);
    old.alive = false;
    await h.bridge.close();
    assert.equal((await commands(h, thread)).availability, "unavailable");
  } finally { await h.cleanup(); }
});

test("a late create receipt preserves its native ID without publishing a replaced peer's catalog", async () => {
  const h = await harness({ commandScript: [{ before: [{ commands: commandRows }] }] });
  let releaseReceipt;
  try {
    const old = await h.bridge.agent();
    const request = old.request.bind(old);
    let received;
    const accepted = new Promise(resolve => { received = resolve; });
    const gate = new Promise(resolve => { releaseReceipt = resolve; });
    old.request = async (method, params, options) => {
      const response = await request(method, params, options);
      if (method === "session/new") { received(response); await gate; }
      return response;
    };
    const pending = h.start();
    const receipt = await within(accepted);
    await old.close();
    const current = await h.bridge.agent();
    releaseReceipt();
    const { thread } = await pending;
    assert.equal(thread.sessionId, receipt.sessionId, "a confirmed native create remains recoverable");
    const read = await commands(h, thread);
    assert.equal(read.availability, "unavailable");
    assert.equal(read.commands, null);
    assert.equal(read.peerId, null);
    assert.equal((await current.request("test/state", {})).controls.length, 0, "reading did not attach the replacement");
    const saved = await new Sessions(join(h.root, "sessions")).initialize();
    assert.equal(saved.get(thread.id).acpSessionId, receipt.sessionId);
    const { turn } = await h.call("turn/start", { threadId: thread.id, input: [{ type: "text", text: "hi" }] });
    await h.turnDone(turn.id);
    assert.deepEqual((await commands(h, thread)).commands, commandRows);
    assert.equal((await current.request("test/state", {})).controls.filter(c => c.method === "session/resume").length, 1);
  } finally { releaseReceipt?.(); await h.cleanup(); }
});


// Observable contract: real provider deltas cannot hide behind blocked sends.
// The existing streaming behavior owner also checks receipts, durable outcomes,
// ordered complete/error delivery, shared overflow and bounded close.
for (const fail of [false, true]) {
  test(`a blocked burst below the bound drains in order with ${fail ? "the native error" : "its complete result"}`, async t => {
    let unblock;
    const gate = new Promise(resolve => { unblock = resolve; });
    let blocked = false, writes = 0;
    const h = await harness({ outputLimits: { bytes: 1024 * 1024, items: 128 }, sendGate: async () => {
      if (blocked) { writes++; await gate; }
    } });
    try {
      const { thread } = await h.start();
      const { turn } = await h.call("turn/start", { threadId: thread.id,
        input: [{ type: "text", text: `burst ${JSON.stringify({ count: 80, size: 512, fail })}` }] });
      blocked = true;
      await until(() => h.bridge.sessions.get(thread.id).turns[0].status !== "inProgress");
      await h.bridge.sessions.flush();
      const stored = JSON.parse(await readFile(join(h.root, "sessions", `${thread.id}.json`), "utf8"));
      const expected = Array.from({ length: 80 }, (_, i) => `${i}:` + "é".repeat(512));
      assert.equal(stored.turns[0].status, fail ? "failed" : "completed");
      assert.equal(stored.turns[0].items[1].text, expected.join(""));
      assert.equal(h.frames.some(f => f.method === "turn/started"), false);
      assert.equal(writes, 1);
      const counts = { bytes: h.bridge.output.bytes, items: h.bridge.output.items };
      assert.equal(counts.items, fail ? 86 : 84); // a failed turn also queues its error item
      assert.ok(counts.bytes < 1024 * 1024);
      t.diagnostic(JSON.stringify({ phase: "blocked", nativeDeltas: 80, sinkCalls: writes, queued: counts, durableStatus: stored.turns[0].status }));
      unblock();
      const done = await h.turnDone(turn.id);
      await h.bridge.output.flush();
      assert.deepEqual(h.frames.filter(f => f.method === "item/agentMessage/delta").map(f => f.params.delta), expected);
      const events = h.frames.filter(f => f.params?.turnId === turn.id);
      assert.deepEqual(events.map(f => f.method), ["turn/started", "item/started", ...expected.map(() => "item/agentMessage/delta"), "item/completed", ...(fail ? ["item/started", "item/completed"] : []), "turn/completed"]);
      const receipt = h.frames.findIndex(f => f.result?.turn?.id === turn.id);
      assert.ok(receipt >= 0 && receipt < h.frames.indexOf(events[0]));
      assert.equal(h.frames[receipt].result.turn.status, "inProgress");
      assert.equal(done.status, stored.turns[0].status);
      assert.equal(done.items[0].text, expected.join(""));
      if (fail) assert.match(done.error.message, /Synthetic burst failed/);
      assert.deepEqual(h.errors, []);
      assert.equal(await h.bridge.close(), true);
      t.diagnostic(JSON.stringify({ phase: "unblocked", deliveredEvents: events.length, terminalStatus: done.status, queuedBytes: h.bridge.output.bytes, queuedItems: h.bridge.output.items }));
    } finally { unblock(); await h.cleanup(); }
  });
}

for (const [kind, limits, size] of [["bytes", { bytes: 128 * 1024, items: 4096 }, 1024], ["items", { bytes: 1024 * 1024, items: 64 }, 8]]) {
  test(`blocked streaming output has one shared ${kind} admission bound across turns`, async t => {
    let unblock;
    const gate = new Promise(resolve => { unblock = resolve; });
    let blocked = false, sends = 0, generatedBytes = 0, generatedItems = 0;
    const h = await harness({ outputLimits: limits, sendGate: async () => {
      if (blocked) { sends++; await gate; }
    } });
    try {
      const threads = [(await h.start()).thread, (await h.start()).thread];
      blocked = true;
      const turns = [];
      for (const thread of threads) {
        // Each burst alone is below the bound. Together they must overflow,
        // even though only one write can reach the blocked shared sink.
        const { turn } = await h.bridge.request("turn/start", { threadId: thread.id,
          input: [{ type: "text", text: `burst ${JSON.stringify({ count: 40, size, hold: true })}` }] });
        turns.push(turn);
        const projection = h.bridge.active.get(thread.id).projection;
        const emit = projection.emit;
        projection.emit = frame => {
          if (!h.bridge.output?.failure) { generatedBytes += Buffer.byteLength(JSON.stringify(frame) + "\n"); generatedItems++; }
          emit(frame);
        };
      }
      await until(async () => h.errors.length || Object.values((await h.agents[0].request("test/state", {})).streams).reduce((a, b) => a + b, 0) === 80);
      t.diagnostic(JSON.stringify({ phase: "blocked", generatedBytes, generatedItems, sinkCalls: sends,
        errors: h.errors.map(e => ({ message: e.message, queuedBytes: e.queuedBytes, queuedItems: e.queuedItems, rejectedBytes: e.rejectedBytes })) }));
      const overflow = h.errors.find(e => /output exceeded/.test(e.message));
      assert.ok(overflow, "combined stream bursts must fail at admission while send is blocked");
      assert.ok(overflow.queuedBytes <= limits.bytes);
      assert.ok(overflow.queuedItems <= limits.items);
      assert.equal(generatedItems, overflow.queuedItems + 1);
      assert.equal(generatedBytes, overflow.queuedBytes + overflow.rejectedBytes);
      if (kind === "items") assert.equal(overflow.queuedItems, limits.items);
      else assert.ok(overflow.queuedBytes + overflow.rejectedBytes > limits.bytes);
      assert.equal(sends, 1);
      const started = performance.now();
      await within(h.bridge.close(), 5500);
      assert.equal(h.agents[0].exitNotified, true, "the exact owned shared process must exit");
      const saved = await new Sessions(join(h.root, "sessions")).initialize();
      for (const [index, thread] of threads.entries()) assert.equal(saved.get(thread.id).turns.find(value => value.id === turns[index].id).status, "failed");
      const delivered = h.frames.length;
      unblock();
      await assert.rejects(h.bridge.output.flush(), /output exceeded/);
      assert.ok(h.frames.length <= delivered + 1, "failed queued frames must not write after unblock");
      assert.equal(h.frames.some(f => f.method === "turn/completed"), false, "failed transport cannot claim delivery of a terminal result");
      assert.ok(h.errors.every(e => e === overflow), "every observed failure must be the latched transport cause");
      t.diagnostic(JSON.stringify({ closeMs: performance.now() - started, statuses: threads.map(thread => saved.get(thread.id).turns[0].status), deliveredAfterFailure: h.frames.length - delivered }));
    } finally { unblock(); await h.cleanup(); }
  });
}

test("closing a below-bound blocked stream stays bounded and persists its real outcome", async t => {
  let unblock;
  const gate = new Promise(resolve => { unblock = resolve; });
  let blocked = false;
  const h = await harness({ sendGate: async () => { if (blocked) await gate; } });
  try {
    const { thread } = await h.start();
    const { turn } = await h.call("turn/start", { threadId: thread.id,
      input: [{ type: "text", text: 'burst {"count":32,"size":128,"hold":true}' }] });
    blocked = true;
    await until(async () => (await h.agents[0].request("test/state", {})).streams[thread.sessionId] === 32);
    const started = performance.now();
    assert.equal(await within(h.bridge.close(), 5500), true, "owned process exit and durable cleanup finish independently of output");
    assert.ok(h.bridge.output.items > 0, "delivery is still blocked until the sink resumes");
    assert.ok(performance.now() - started < 5000);
    const saved = await new Sessions(join(h.root, "sessions")).initialize();
    assert.equal(saved.get(thread.id).turns[0].status, "failed");
    unblock();
    assert.equal((await h.turnDone(turn.id)).status, "failed");
    await h.bridge.output.flush();
    t.diagnostic(JSON.stringify({ closeMs: performance.now() - started, observedOutcome: "failed", queuedBytes: h.bridge.output.bytes, queuedItems: h.bridge.output.items }));
  } finally { unblock(); await h.cleanup(); }
});

// serverCall shares the same ordered transport as projections. Overflow must
// also retire an approval waiting behind a blocked item frame.
test("output overflow cancels a queued approval and preserves both owned outcomes", async t => {
  let unblock;
  const gate = new Promise(resolve => { unblock = resolve; });
  let blocked = false;
  const h = await harness({ outputLimits: { bytes: 128 * 1024, items: 4096 }, sendGate: async () => { if (blocked) await gate; } });
  try {
    const command = (await h.start()).thread, burst = (await h.start()).thread;
    await h.call("turn/start", { threadId: command.id, input: [{ type: "text", text: "command" }] });
    blocked = true;
    await until(() => h.bridge.pending.size === 1);
    await h.bridge.request("turn/start", { threadId: burst.id,
      input: [{ type: "text", text: 'burst {"count":100,"size":1024,"hold":true}' }] });
    await until(() => h.errors.length);
    await within(h.bridge.close(), 5500);
    assert.equal(h.bridge.pending.size, 0);
    assert.equal(h.agents[0].exitNotified, true);
    const saved = await new Sessions(join(h.root, "sessions")).initialize();
    assert.deepEqual([command, burst].map(thread => saved.get(thread.id).turns[0].status), ["failed", "failed"]);
    assert.equal(h.frames.some(frame => frame.method?.endsWith("/requestApproval")), false);
    assert.ok(h.errors.every(error => error === h.errors[0]));
    unblock();
    await assert.rejects(h.bridge.output.flush(), /output exceeded/);
    assert.equal(h.frames.some(frame => frame.method?.endsWith("/requestApproval")), false, "an approval cannot arrive after transport failure");
    t.diagnostic(JSON.stringify({ queuedApprovalCancelled: true, pending: h.bridge.pending.size, durableStatuses: ["failed", "failed"] }));
  } finally { unblock(); await h.cleanup(); }
});

// The daemon names the driver when it creates the thread and again with each
// turn. It reaches the session through the session's own server (never a
// folder-wide one); a later change reopens the session and is kept.
test("cua driver reaches a new OpenCode session and later changes reattach it", async () => {
  const h = await harness();
  try {
    const on = { "mcp_servers.cua_driver": { enabled: true, command: join(h.root, "missing-cua-driver"), args: ["mcp"] } };
    const off = { "mcp_servers.cua_driver": { enabled: false } };
    const { thread } = await h.call("thread/start", { requestId: randomUUID(), cwd: h.root, model: "opencode:opencode/free-model",
      wonderPolicy: { mode: "workspace", approvalMode: "ask" }, wonderProject: { cwd: h.root, additionalDirectories: [] }, config: on });
    const turn = async config => {
      const { turn } = await h.call("turn/start", { threadId: thread.id, input: [{ type: "text", text: "hi" }], config });
      await h.turnDone(turn.id);
    };
    const controls = async () => (await h.agents.at(-1).request("test/state", {})).controls.filter(c => c.mcpServers);
    await turn(on);
    assert.deepEqual(await controls(), [{ method: "session/new", mcpServers: [serverName(thread.id)] }]);
    await turn(off);
    assert.deepEqual((await controls()).slice(1).map(c => [c.method, c.mcpServers]), [["session/resume", []]]);
    await turn(off);
    assert.equal((await controls()).length, 2, "an unchanged driver does not reopen the session");
    const saved = await new Sessions(join(h.root, "sessions")).initialize();
    assert.deepEqual(saved.get(thread.id).options.config, off);
  } finally { await h.cleanup(); }
});

// Cursor advertises parameterized model values, agent/plan modes, explicit
// authentication and load-only attachment. Exercise these through real pipes
// and the same admission/approval/cancel owners as OpenCode.
test("Cursor keeps its identity, exact model, auth and modes through streaming and reload", async () => {
  const h = await harness({ adapter: cursorAdapter });
  try {
    assert.equal((await h.call("initialize")).wonderBridge.family, "cursor");
    const { thread } = await h.start({ mode: "workspace", approvalMode: "ask", planMode: true });
    assert.match(thread.id, /^cursor-/);
    assert.equal(thread.agentFamily, "cursor");
    const params = { threadId: thread.id, clientUserMessageId: "cursor-1", input: [{ type: "text", text: "hi" }] };
    const { turn } = await h.call("turn/start", params);
    assert.equal((await h.turnDone(turn.id)).status, "completed");
    const native = await h.agents[0].request("test/state", {});
    assert.equal(native.controls[0].method, "authenticate");
    assert.equal(native.prompts[0].model, "gpt-5.6-luna[context=272k,reasoning=medium,fast=false]");
    assert.equal(native.prompts[0].mode, "plan");
    assert.ok(h.frames.filter(frame => frame.params?.turnId === turn.id).every(frame => frame.params.agentFamily === "cursor"));
    assert.equal((await h.call("turn/start", params)).turn.id, turn.id);
    await h.restart();
    const next = await h.call("turn/start", { threadId: thread.id, wonderPolicy: { mode: "workspace", approvalMode: "ask", planMode: false }, input: [{ type: "text", text: "hi" }] });
    assert.equal((await h.turnDone(next.turn.id)).status, "completed");
    const loaded = await h.agents[1].request("test/state", {});
    assert.ok(loaded.controls.some(control => control.method === "session/load"));
    assert.ok(!loaded.controls.some(control => control.method === "session/resume"));
    assert.equal(loaded.prompts[0].mode, "agent");
    assert.equal((await h.call("thread/read", { threadId: thread.id, includeTurns: true })).thread.turns.length, 2);
  } finally { await h.cleanup(); }
});

for (const decision of ["accept", "decline", { acpPermission: { optionId: "always", kind: "allow_always" } }, "cancel"]) {
  test(`Cursor approval retains native options: ${JSON.stringify(decision)}`, async () => {
    const h = await harness({ adapter: cursorAdapter, approve: decision });
    try {
      const { thread } = await h.start();
      const { turn } = await h.call("turn/start", { threadId: thread.id, input: [{ type: "text", text: "run a command" }] });
      const done = await h.turnDone(turn.id);
      const approval = h.frames.find(frame => frame.method?.endsWith("/requestApproval"));
      assert.equal(approval.params.agentFamily, "cursor");
      assert.match(approval.params.reason, /Cursor/);
      assert.deepEqual(approval.params.availableDecisions, ["accept", { acpPermission: { optionId: "always", kind: "allow_always" } }, "decline", "cancel"]);
      const native = await h.agents[0].request("test/state", {});
      const expected = decision === "accept" ? "once" : decision === "decline" ? "reject" : typeof decision === "object" ? "always" : null;
      assert.deepEqual(native.permissionAnswers[0], expected ? { outcome: "selected", optionId: expected } : { outcome: "cancelled" });
      const allowed = expected === "once" || expected === "always";
      assert.equal(done.items.find(item => item.type === "commandExecution").status, allowed ? "completed" : "failed");
    } finally { await h.cleanup(); }
  });
}

test("Cursor Stop waits for its native cancelled result and reloads provider-specific uncertainty", async () => {
  const h = await harness({ adapter: cursorAdapter });
  try {
    const { thread } = await h.start();
    const { turn } = await h.call("turn/start", { threadId: thread.id, input: [{ type: "text", text: "wait" }] });
    await until(() => h.frames.some(frame => frame.method === "item/agentMessage/delta"));
    assert.deepEqual(await h.call("turn/interrupt", { threadId: thread.id, turnId: turn.id }), { turn: { id: turn.id, status: "interrupted" } });
    assert.equal((await h.turnDone(turn.id)).status, "interrupted");
    const native = await h.agents[0].request("test/cancelled-sessions", {});
    assert.deepEqual(native.sessions, [thread.sessionId]);
    const loaded = await new Sessions(join(h.root, "sessions"), cursorAdapter).initialize();
    assert.equal(loaded.get(thread.id).turns[0].status, "interrupted");
    const opencode = await new Sessions(join(h.root, "sessions")).initialize();
    assert.equal(opencode.values.size, 0, "another provider cannot ingest the Cursor journal");
  } finally { await h.cleanup(); }
});

// Cursor's to-do extension reaches the same checklist as an ACP plan, through
// the real pipe and the running reply's session, merging by to-do ID.
test("Cursor to-do updates become the turn's checklist", async () => {
  const h = await harness({ adapter: cursorAdapter });
  try {
    const { thread } = await h.start();
    const { turn } = await h.call("turn/start", { threadId: thread.id, clientUserMessageId: "todos", input: [{ type: "text", text: "cursor todos" }] });
    const done = await h.turnDone(turn.id);
    assert.equal(done.status, "completed");
    const lists = h.frames.filter(f => f.method === "item/completed" && f.params.item.type === "taskChecklist").map(f => f.params.item.taskChecklist);
    assert.deepEqual(lists.map(l => l.entries), [
      [{ text: "Read the parser", status: "inProgress" }, { text: "Fix the bug", status: "pending" }],
      [{ text: "Read the parser", status: "completed" }, { text: "Fix the bug", status: "pending" }]]);
    assert.equal(lists[1].completed, 1);
    assert.equal(done.items.filter(i => i.type === "taskChecklist").length, 1);
    assert.equal(h.agents[0].prompting.size, 0, "a finished reply no longer receives unaddressed to-dos");
    const before = h.frames.length;
    h.agents[0].todoUpdate({ toolCallId: "unknown", todos: [{ content: "Orphan" }] });
    assert.equal(h.frames.length, before, "an update no reply owns is not shown anywhere");
  } finally { await h.cleanup(); }
});

// Two Cursor replies at once: an update that names no session follows the
// tool call that made it, so each chat gets its own whole list.
test("concurrent Cursor replies each keep their own to-do list", async () => {
  const h = await harness({ adapter: cursorAdapter });
  try {
    const threads = [(await h.start()).thread, (await h.start()).thread];
    const turns = await Promise.all(threads.map((thread, index) => h.call("turn/start", { threadId: thread.id,
      clientUserMessageId: `pair-${index}`, input: [{ type: "text", text: `paired todos ${"AB"[index]}` }] })));
    const done = await Promise.all(turns.map(({ turn }) => h.turnDone(turn.id)));
    assert.equal(h.agents.length, 1, "both replies share one Cursor process");
    for (const [index, { turn }] of turns.entries()) {
      const name = "AB"[index];
      assert.equal(done[index].status, "completed");
      const lists = h.frames.filter(f => f.method === "item/completed" && f.params.turnId === turn.id && f.params.item.type === "taskChecklist")
        .map(f => f.params.item.taskChecklist);
      assert.deepEqual(lists.map(l => l.entries), [
        [{ text: `${name} first`, status: "inProgress" }, { text: `${name} second`, status: "pending" }],
        [{ text: `${name} first`, status: "completed" }, { text: `${name} second`, status: "pending" }]]);
      assert.deepEqual([lists[1].total, lists[1].completed], [2, 1], "a merge is never shown as the whole list");
    }
  } finally { await h.cleanup(); }
});

test("OpenCode thinking text is kept as finished reasoning segments", async () => {
  const { TurnProjection } = await import("../projection.mjs");
  const events = [];
  const p = new TurnProjection({ threadId: "t", turnId: "turn", emit: e => events.push(e) }); p.start();
  const thought = text => p.update({ sessionUpdate: "agent_thought_chunk", content: { type: "text", text } });
  thought("Look at the parser.\n\nThen run"); thought(" the tests.");
  const started = events.filter(e => e.method === "item/started" && e.params.item.type === "reasoning");
  assert.deepEqual(started.map(e => e.params.item), [{ type: "reasoning", id: "turn:thinking:1", status: "inProgress" }], "text travels only with the finished segment");
  p.update({ sessionUpdate: "agent_message_chunk", content: { type: "text", text: "Checking." } });
  thought("Second thought");
  p.update({ sessionUpdate: "tool_call", toolCallId: "r", kind: "read", title: "read", status: "completed", rawInput: {} });
  thought("Cut off");
  const result = p.finish({ stopReason: "cancelled" });
  const reasoning = result.items.filter(i => i.type === "reasoning");
  assert.deepEqual(reasoning, [
    { type: "reasoning", id: "turn:thinking:1", status: "completed", summary: ["Look at the parser.", "Then run the tests."] },
    { type: "reasoning", id: "turn:thinking:2", status: "completed", summary: ["Second thought"] },
    { type: "reasoning", id: "turn:thinking:3", status: "interrupted", summary: ["Cut off"] }]);
  assert.equal(result.items.find(i => i.type === "agentMessage").phase, "commentary");
  // A thought chunk without text is timing only.
  const q = new TurnProjection({ threadId: "t", turnId: "q", emit: () => {} }); q.start();
  q.update({ sessionUpdate: "agent_thought_chunk", content: { type: "image" } });
  assert.ok(q.firstTokenAt != null);
  assert.equal(q.finish({ stopReason: "end_turn" }).items.length, 0);
});

test("OpenCode sub-agents and to-do writes get their own activity", async () => {
  const { TurnProjection } = await import("../projection.mjs");
  const events = [];
  const p = new TurnProjection({ threadId: "t", turnId: "turn", emit: e => events.push(e), now: () => 1000 }); p.start();
  p.update({ sessionUpdate: "tool_call", toolCallId: "a", kind: "think", title: "task", status: "in_progress",
    rawInput: { description: "Audit the parser", prompt: "Long brief", subagent_type: "general" } });
  assert.deepEqual(events.at(-1).params.item.toolAction, { kind: "spawnAgent", description: "Audit the parser" });
  const todos = [{ id: "1", content: "Write tests", status: "completed", priority: "high" }, { id: "2", content: "Fix bug", status: "in_progress" }];
  p.update({ sessionUpdate: "tool_call", toolCallId: "w", kind: "other", title: "todowrite", status: "in_progress", rawInput: { todos } });
  assert.equal(events.at(-1).params.item.toolAction.kind, "updateTasks");
  assert.ok(!events.some(e => e.params.item?.type === "taskChecklist"), "an unconfirmed write is not the checklist");
  p.update({ sessionUpdate: "tool_call_update", toolCallId: "w", status: "completed", rawOutput: { output: "[]", metadata: { todos } } });
  const lists = () => events.filter(e => e.method === "item/completed" && e.params.item.type === "taskChecklist");
  assert.deepEqual(lists().at(-1).params.item.taskChecklist, { entries: [{ text: "Write tests", status: "completed" }, { text: "Fix bug", status: "inProgress" }],
    total: 2, completed: 1, truncated: false });
  p.update({ sessionUpdate: "tool_call", toolCallId: "f", kind: "other", title: "todowrite", status: "in_progress",
    rawInput: { todos: [{ content: "Fix bug", status: "completed" }] } });
  p.update({ sessionUpdate: "tool_call_update", toolCallId: "f", status: "failed", content: [{ type: "content", content: { type: "text", text: "Invalid" } }] });
  assert.equal(lists().length, 1, "a failed write leaves the checklist unchanged");
  assert.equal(events.at(-1).params.item.status, "failed");
  p.update({ sessionUpdate: "tool_call", toolCallId: "m", kind: "other", title: "todowrite", status: "completed", rawInput: { todos: "not a list" } });
  assert.equal(lists().length, 1, "a malformed list is not a checklist");
  // Another provider's tool named task without OpenCode's input stays generic.
  p.update({ sessionUpdate: "tool_call", toolCallId: "g", kind: "think", title: "task", status: "completed", rawInput: { prompt: "x" } });
  assert.equal(events.at(-1).params.item.toolAction.kind, "think");
});

for (const label of ["OpenCode", "Cursor"]) {
  test(`${label} code errors after an edit stay on a completed file change`, async () => {
    const { toolItem } = await import("../projection.mjs");
    const error = (line, message, severity = 1) => ({ range: { start: { line, character: 4 }, end: { line, character: 9 } }, severity, message });
    const edit = (diagnostics, status = "completed") => toolItem({ toolCallId: "e", kind: "edit", status,
      content: [{ type: "diff", path: "/p/main.ts", oldText: "a", newText: "b" }], rawOutput: { output: "Edit applied", metadata: { diagnostics } } }, label);
    const item = edit({ "/p/main.ts": [error(0, "Type 'string' is not assignable to type 'number'."), error(1, "Unused", 2), error(2, "x"), error(3, "y"), error(4, "z")],
      "/p/other.ts": [error(0, "Not changed by this edit")] });
    assert.equal(item.status, "completed");
    assert.equal(item.error, undefined, "an error key would mark the edit failed");
    assert.equal(item.diagnosticsReporter, label);
    assert.deepEqual(item.diagnostics, [{ path: "/p/main.ts", line: 1, column: 5, message: "Type 'string' is not assignable to type 'number'." },
      { path: "/p/main.ts", line: 3, column: 5, message: "x" }, { path: "/p/main.ts", line: 4, column: 5, message: "y" }]);
    assert.equal(edit({}).diagnostics, undefined);
    assert.equal(edit({ "/p/main.ts": [] }).diagnostics, undefined);
    assert.equal(edit(undefined).diagnostics, undefined);
    assert.equal(edit({ "/p/main.ts": "broken" }).diagnostics, undefined);
    assert.equal(edit({ "/p/main.ts": [{ severity: 1, message: "no range" }, { ...error(-1, "negative") }, error(0, "   ")] }).diagnostics, undefined);
    assert.equal(edit({ "/p/main.ts": [error(0, "e")] }, "failed").diagnostics, undefined);
    assert.equal(edit({ "/p/main.ts": [error(0, "m".repeat(5000))] }).diagnostics[0].message.length, 512);
    const many = toolItem({ toolCallId: "e", kind: "edit", status: "completed",
      content: Array.from({ length: 5 }, (_, i) => ({ type: "diff", path: `/p/${i}.ts`, oldText: "a", newText: "b" })),
      rawOutput: { metadata: { diagnostics: Object.fromEntries(Array.from({ length: 5 }, (_, i) => [`/p/${i}.ts`, [error(0, "a"), error(1, "b"), error(2, "c")]])) } } }, label);
    assert.equal(many.diagnostics.length, 10);
    // A write names its file in the input, not a diff block.
    const write = toolItem({ toolCallId: "w", kind: "edit", status: "completed", rawInput: { filePath: "/p/new.ts", content: "let x: number = 'a'\n" },
      rawOutput: { output: "Wrote file successfully.\n\nLSP errors detected in this file, please fix:", metadata: { diagnostics: { "/p/new.ts": [error(0, "Type 'string' is not assignable to type 'number'.")] } } } }, label);
    assert.deepEqual(write.diagnostics, [{ path: "/p/new.ts", line: 1, column: 5, message: "Type 'string' is not assignable to type 'number'." }]);
    assert.equal(write.status, "completed");
  });
}

test("Cua tools are offered to tool-limited models only on an explicit computer-use request, and kept once offered", () => {
  const ask = text => [{ type: "text", text }];
  const beam = "opencode/beam-free";
  assert.equal(offersComputerUse("opencode/other", ask("hello")), true);
  assert.equal(offersComputerUse(beam, ask("why does the simulator crash on launch")), false);
  assert.equal(offersComputerUse(beam, ask("take a screenshot of the desktop")), false);
  assert.equal(offersComputerUse(beam, null), false);
  assert.equal(offersComputerUse(beam, ask("use computer use to open Notes")), true);
  assert.equal(offersComputerUse(beam, ask("thanks"), true), true);
  // A thread that had the tools on a roomy model holds nothing: only a limited model's request is held.
  assert.equal(offersComputerUse(beam, ask("thanks"), false), false);
  assert.equal(offersComputerUse("opencode/deepseek-flash", ask("hi")), true);
});

test("a tool-limited model gets a compact Cua tool subset; other models get them all", () => {
  const all = ["click", "type_text", "browser_click", "set_agent_cursor_theme", "run_script", "run_steps"].map(name => ({ name }));
  assert.deepEqual(limitedComputerTools("opencode/beam-free", all).map(tool => tool.name), ["click", "type_text", "run_steps"]);
  assert.equal(limitedComputerTools("opencode/deepseek-flash", all).length, 6);
});

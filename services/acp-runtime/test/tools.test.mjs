import { test } from "node:test";
import assert from "node:assert/strict";
import { copyFile, mkdir, mkdtemp, readFile, readdir, rm, writeFile } from "node:fs/promises";
import { createConnection } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { randomUUID } from "node:crypto";
import { fileURLToPath, pathToFileURL } from "node:url";
import { AcpAgent } from "../agent.mjs";
import { AcpBridge } from "../bridge.mjs";
import { Sessions } from "../sessions.mjs";
import { CuaDriver, ToolServer, ToolTokens, qualifiedName, serverName, wonderTool } from "../tool-server.mjs";
import { PROJECT_TOOLS, loadProjectTools } from "../thread-tools.mjs";

// Cupola's thread tools reach an ACP agent as a per-session stdio MCP server.
// The fake agent registers servers per process by name, as OpenCode 1.18.27
// does per folder, so every session sees every server.
const fake = fileURLToPath(new URL("./fake-agent.mjs", import.meta.url));
const { names: NAMES } = JSON.parse(await readFile(new URL("../../../packages/protocol/thread-tools.json", import.meta.url), "utf8"));
// OpenCode and Cursor report an MCP call differently (fake-agent.mjs); each
// style must reach the same thread-tool contract.
const STYLES = ["opencode", "cursor"];
const specs = (note = "") => NAMES.map(name => ({ type: "function", name, description: `${name}${note}`, inputSchema: { type: "object", properties: {} } }));

// The daemon's own answers: the owner is not using the computer, and an
// empty thread list for Wonder's tools.
const daemonAnswer = params => ({ success: true, contentItems: [{ type: "inputText",
  text: params.tool === "wonder_computer_control_check" ? "{\"ownerHasControl\":false}" : "{\"threads\":[]}" }] });
async function harness({ nativePermission = "allow", toolStyle = "opencode", tools = {}, matchMs = 300, answer = daemonAnswer, env = {} } = {}) {
  const root = await mkdtemp(join(tmpdir(), "wonder-acp-tools-"));
  const frames = [], agents = [], issued = [];
  let bridge;
  const send = async frame => {
    frames.push(frame);
    if (frame.method === "item/tool/call") setImmediate(() => bridge.receive({ id: frame.id, result: answer(frame.params) }));
    if (frame.method?.endsWith("/requestApproval")) setImmediate(() => bridge.receive({ id: frame.id, result: { decision: "accept" } }));
  };
  const sessions = await new Sessions(join(root, "sessions")).initialize();
  bridge = new AcpBridge({ sessions, send, tools, matchMs,
    inspect: async () => ({ installed: true, version: "1.0.0", models: [{ id: "opencode/free-model", name: "Free Model", providerId: "opencode", free: true }], defaultModel: null }),
    createAgent: handlers => {
      const agent = new AcpAgent({ command: process.execPath, args: [fake], env: { WONDER_FAKE_NATIVE_PERMISSION: nativePermission, WONDER_FAKE_TOOL_STYLE: toolStyle, ...env }, ...handlers });
      agents.push(agent); return agent;
    } });
  // Keep each raw credential so the test can prove where it never appears.
  const issue = bridge.tools.tokens.issue.bind(bridge.tools.tokens);
  bridge.tools.tokens.issue = (...args) => { const token = issue(...args); issued.push(token); return token; };
  const call = async (method, params) => {
    const id = `t-${randomUUID()}`;
    await bridge.receive({ id, method, params });
    const reply = frames.find(f => f.id === id && !f.method);
    if (reply.error) throw new Error(reply.error.message);
    return reply.result;
  };
  const turnDone = async turnId => {
    for (let i = 0; i < 1000; i++) {
      const done = frames.find(f => f.method === "turn/completed" && f.params.turnId === turnId);
      if (done) return done.params.turn;
      await new Promise(resolve => setTimeout(resolve, 10));
    }
    throw new Error("turn did not complete");
  };
  const start = (dynamicTools = specs()) => call("thread/start", { requestId: randomUUID(), cwd: root, model: "opencode:opencode/free-model",
    wonderPolicy: { mode: "workspace", approvalMode: "ask" }, wonderProject: { cwd: root, additionalDirectories: [] }, dynamicTools });
  const ask = async (thread, request, extra = {}) => {
    const { turn } = await call("turn/start", { threadId: thread.id, clientUserMessageId: randomUUID(), input: [{ type: "text", text: `call ${JSON.stringify(request)}` }], ...extra });
    const done = await turnDone(turn.id);
    return { turn, done, reply: JSON.parse(done.items.find(i => i.type === "agentMessage").text) };
  };
  const state = method => agents.at(-1).request(method, {});
  return { root, frames, agents, issued, bridge, call, start, ask, state, turnDone,
    cleanup: async () => { await bridge.close(); await rm(root, { recursive: true, force: true }); } };
}

async function until(check, ms = 3000) {
  const deadline = Date.now() + ms;
  for (;;) {
    const value = await check();
    if (value) return value;
    if (Date.now() > deadline) throw new Error("condition not reached");
    await new Promise(resolve => setTimeout(resolve, 20));
  }
}

for (const toolStyle of STYLES) test(`session/new carries the thread's own Cupola server and a call round-trips through the daemon (${toolStyle})`, async () => {
  const h = await harness({ toolStyle });
  try {
    const { thread } = await h.start();
    const own = serverName(thread.id);
    const { controls } = await h.state("test/state");
    assert.deepEqual(controls.find(c => c.method === "session/new").mcpServers, [own]);
    const [server] = (await h.state("test/mcp")).servers;
    assert.equal(server.name, own);
    assert.deepEqual(server.tools, [], "an idle thread offers no tools");
    assert.equal(server.listChanged, true);
    const { turn, done, reply } = await h.ask(thread, { server: "own", tool: "wonder_thread_list", arguments: { limit: 2 } });
    assert.deepEqual(reply, { content: [{ type: "text", text: "{\"threads\":[]}" }], isError: false });
    const calls = h.frames.filter(f => f.method === "item/tool/call");
    assert.equal(calls.length, 1);
    const item = done.items.find(i => i.type === "mcpToolCall");
    assert.deepEqual(calls[0].params, { threadId: thread.id, turnId: turn.id, callId: item.id, tool: "wonder_thread_list", arguments: { limit: 2 }, agentFamily: "opencode" });
    // The activity names Cupola's tool, not the per-session server name.
    assert.equal(item.server, "Cupola");
    assert.equal(item.tool, "wonder_thread_list");
    // The credential reached only the agent: not the daemon, not the journal.
    assert.equal(h.issued.length, 1);
    // A save in flight can rename its temporary file away between the listing and the read.
    const journal = await Promise.all((await readdir(join(h.root, "sessions"))).map(file => readFile(join(h.root, "sessions", file), "utf8")
      .catch(error => { if (error.code === "ENOENT") return ""; throw error; })));
    for (const text of [...journal, JSON.stringify(h.frames)]) assert.equal(text.includes(h.issued[0]), false);
  } finally { await h.cleanup(); }
});

// Contract: a visual's page, larger than 8 MB once JSON-escaped, reaches the
// daemon whole through the session's server and is reported as Cupola's tool.
test("a wonder_show_visual call carries its whole page to the daemon", async () => {
  const h = await harness();
  try {
    const { thread } = await h.start();
    const html = `<p>${"\"".repeat(4.5 * 1024 * 1024)}</p>`;
    const { done, reply } = await h.ask(thread, { server: "own", tool: "wonder_show_visual", arguments: { title: "Quotes", html } });
    assert.equal(reply.isError, false);
    const [call] = h.frames.filter(f => f.method === "item/tool/call");
    assert.equal(call.params.tool, "wonder_show_visual");
    assert.equal(call.params.arguments.html, html);
    assert.ok(JSON.stringify(call.params.arguments).length > 8 * 1024 * 1024);
    const item = done.items.find(i => i.type === "mcpToolCall");
    assert.deepEqual([item.server, item.tool], ["Cupola", "wonder_show_visual"]);
  } finally { await h.cleanup(); }
});

for (const toolStyle of STYLES) test(`a thread that calls another thread's server is refused and pointed at its own (${toolStyle})`, async () => {
  const h = await harness({ toolStyle });
  try {
    const a = (await h.start()).thread, b = (await h.start()).thread;
    assert.deepEqual((await h.state("test/mcp")).servers.map(s => s.name).sort(), [serverName(a.id), serverName(b.id)].sort());
    const { reply } = await h.ask(a, { server: serverName(b.id), tool: "wonder_thread_list", arguments: {} });
    assert.equal(reply.isError, true);
    assert.match(reply.content[0].text, new RegExp(`another thread in this folder. Use ${serverName(a.id)}_wonder_thread_list instead`));
    assert.equal(h.frames.some(f => f.method === "item/tool/call"), false);
  } finally { await h.cleanup(); }
});

for (const toolStyle of STYLES) test(`an agent that asks permission gets its own Cupola tools allowed and another thread's refused, without an approval card (${toolStyle})`, async () => {
  const h = await harness({ toolStyle, nativePermission: "ask" });
  try {
    const a = (await h.start()).thread, b = (await h.start()).thread;
    assert.deepEqual((await h.ask(a, { server: serverName(b.id), tool: "wonder_thread_read", arguments: { threadId: "x" } })).reply, { rejected: true });
    assert.equal((await h.ask(a, { server: "own", tool: "wonder_thread_read", arguments: { threadId: "x" } })).reply.isError, false);
    assert.equal(h.frames.some(f => f.method?.endsWith("/requestApproval")), false);
    assert.equal(h.frames.filter(f => f.method === "item/tool/call").length, 1);
  } finally { await h.cleanup(); }
});

test("a call the agent never reported for this thread does not run", async () => {
  const h = await harness({ matchMs: 150 });
  try {
    const { thread } = await h.start();
    const { reply } = await h.ask(thread, { server: "own", tool: "wonder_thread_list", arguments: {}, report: false });
    assert.equal(reply.isError, true);
    assert.match(reply.content[0].text, /could not match this call/);
    // Outside a reply there is no turn to act for.
    const idle = await h.state("test/mcp").then(() => h.agents[0].request("test/mcp-call", { server: serverName(thread.id), tool: "wonder_thread_list" }));
    assert.match(idle.content[0].text, /only while this thread is replying/);
    assert.equal(h.frames.some(f => f.method === "item/tool/call"), false);
  } finally { await h.cleanup(); }
});

test("an expired credential is refused, its server leaves, and the next reply attaches with a new one", async () => {
  let now = 0;
  const h = await harness({ tools: { ttlMs: 150, now: () => now, sweepMs: 25 } });
  try {
    const { thread } = await h.start();
    const own = serverName(thread.id);
    assert.equal((await h.ask(thread, { server: "own", tool: "wonder_thread_list", arguments: {} })).reply.isError, false);
    assert.equal(h.issued.length, 1);
    // Completion is emitted before persistence and the idle lifetime reset finish.
    await until(() => !h.bridge.active.has(thread.id));
    // Child startup cannot consume the lifetime. Expire only after the first reply is idle.
    now = 151;
    await until(async () => !(await h.state("test/mcp")).servers.some(s => s.name === own));
    assert.equal(h.bridge.tools.tokens.live(thread.id), false);
    const stale = createConnection(h.bridge.tools.path);
    try {
      stale.end(`${JSON.stringify({ type: "hello", token: h.issued[0] })}\n`);
      let response = "";
      for await (const chunk of stale) response += chunk;
      assert.deepEqual(JSON.parse(response), { type: "revoked" });
    } finally { stale.destroy(); }
    assert.deepEqual(await h.agents[0].request("test/mcp-call", { server: own, tool: "wonder_thread_list" }), { missing: true });
    // The process is still attached, but the session is attached again.
    assert.equal((await h.ask(thread, { server: "own", tool: "wonder_thread_list", arguments: {} })).reply.isError, false);
    const { controls } = await h.state("test/state");
    assert.deepEqual(controls.filter(c => c.method === "session/resume").map(c => c.mcpServers), [[own]]);
    assert.equal(h.issued.length, 2);
    assert.notEqual(h.issued[0], h.issued[1]);
    assert.equal(h.frames.filter(f => f.method === "item/tool/call").length, 2);
  } finally { await h.cleanup(); }
});

test("a changed tool list reaches the agent through list_changed", async () => {
  const h = await harness();
  try {
    const { thread } = await h.start();
    const before = (await h.state("test/mcp")).servers[0].lists;
    await h.ask(thread, { server: "own", tool: "wonder_thread_list", arguments: {} }, { dynamicTools: specs(" (changed)") });
    await until(async () => (await h.state("test/mcp")).servers[0].lists > before);
    // The same credential and server remain; only the list was read again.
    assert.equal(h.issued.length, 1);
  } finally { await h.cleanup(); }
});

// Beam refuses a request listing over 128 tools, and OpenCode counts every thread's server in a folder.
// The fake agent only runs a tool its client has listed, and lists slowly after list_changed, so a
// reply that started before the new list arrived would fail its first call.
test("four threads replying at once in one folder stay under 128 tools, and each call routes to its own thread", async () => {
  const h = await harness({ env: { WONDER_FAKE_ACP_STRICT_LIST: "1", WONDER_FAKE_ACP_RELIST_MS: "150" } });
  try {
    const threads = [];
    for (let i = 0; i < 4; i++) threads.push((await h.start()).thread);
    const total = async () => (await h.state("test/mcp")).servers.reduce((sum, s) => sum + s.tools.length, 0);
    assert.equal(await total(), 0, "idle: nothing offered");
    // Hold the daemon's answers so all four replies overlap.
    let release; const gate = new Promise(resolve => { release = resolve; });
    const original = h.bridge.receive.bind(h.bridge);
    h.bridge.receive = async frame => { if (frame.result?.contentItems) await gate; return original(frame); };
    const asks = threads.map(thread => h.ask(thread, { server: "own", tool: "wonder_thread_list", arguments: {} }));
    const peak = await until(async () => { const n = await total(); return n === NAMES.length * 4 ? n : 0; });
    assert.ok(peak < 128, `offered ${peak}`);
    assert.equal((await h.state("test/mcp")).servers.filter(s => s.tools.length === NAMES.length).length, 4, "each replying thread offers its own tools");
    release();
    const results = await Promise.all(asks);
    results.forEach(({ turn, reply }, i) => {
      assert.equal(reply.isError, false, "the first call finds the tool listed");
      const call = h.frames.find(f => f.method === "item/tool/call" && f.params.turnId === turn.id);
      assert.equal(call.params.threadId, threads[i].id);
    });
    await until(async () => (await total()) === 0);
  } finally { await h.cleanup(); }
});

test("a reply start and end tell the agent to list tools again, and only the replying thread lists computer use", async () => {
  const h = await harness();
  try {
    const first = (await h.start()).thread, second = (await h.start()).thread;
    // Each notification is followed by the listing OpenCode would then read.
    const seen = [], original = h.bridge.tools.changed.bind(h.bridge.tools);
    h.bridge.tools.changed = id => {
      original(id);
      seen.push(Promise.all([first, second].map(async t => (await h.bridge.toolRequest(t.id, { type: "list" })).map(tool => tool.name).filter(name => name === "screenshot").length)).then(counts => ({ id, counts })));
    };
    await h.ask(first, { server: "own", tool: "screenshot", arguments: {} }, computer(DRIVER));
    await until(() => seen.length >= 2);
    const snapshots = await Promise.all(seen);
    assert.deepEqual(snapshots.map(s => s.id), [first.id, first.id], "an idle neighbour is not notified");
    assert.deepEqual(snapshots[0].counts, [1, 0], "start: only the replying thread lists computer use");
    assert.deepEqual(snapshots[1].counts, [0, 0], "end: nobody lists it");
    assert.equal(h.bridge.active.has(first.id), false);
  } finally { await h.cleanup(); }
});

test("an exited agent and a closed bridge revoke every credential", async () => {
  const h = await harness();
  try {
    const { thread } = await h.start();
    assert.equal(h.bridge.tools.tokens.live(thread.id), true);
    await h.agents[0].close();
    await until(() => !h.bridge.tools.tokens.live(thread.id));
    const second = (await h.start()).thread;
    assert.equal(h.bridge.tools.tokens.live(second.id), true);
    const path = h.bridge.tools.path;
    await h.bridge.close();
    assert.equal(h.bridge.tools.tokens.records.size, 0);
    await assert.rejects(new Promise((resolve, reject) => createConnection(path).once("connect", resolve).once("error", reject)));
  } finally { await h.cleanup(); }
});

test("a credential is bound to one session, slides while alive and expires when idle", () => {
  let now = 0, alive = false;
  const revoked = [];
  const tokens = new ToolTokens({ ttlMs: 100, now: () => now, alive: () => alive, onRevoke: (_, record) => revoked.push(record.sessionId) });
  const first = tokens.issue("a", "agent-1");
  tokens.issue("b", "agent-1");
  assert.equal(tokens.live("a"), true);
  // A new credential for the same session revokes the old one.
  tokens.issue("a", "agent-2");
  assert.deepEqual(revoked, ["a"]);
  assert.equal([...tokens.records.values()].filter(r => r.sessionId === "a").length, 1);
  assert.notEqual(first, tokens.issue("c", "agent-2"));
  // A running reply keeps a credential alive past its idle lifetime.
  alive = true; now = 500;
  assert.equal(tokens.live("a"), true);
  alive = false; now = 550;
  assert.equal(tokens.live("a"), true);
  now = 700;
  assert.equal(tokens.live("a"), false);
  tokens.revokeOwner("agent-1");
  assert.equal(tokens.live("b"), false);
  tokens.revokeAll();
  assert.equal(tokens.records.size, 0);
});

test("the socket refuses an unknown credential and drops a revoked connection", async () => {
  const server = new ToolServer({ handler: async (sessionId, frame) => ({ sessionId, type: frame.type }), sweepMs: 1000 });
  try {
    const entry = await server.entry("session-1", "owner");
    const token = entry.env.find(e => e.name === "WONDER_TOOLS_TOKEN").value;
    const open = () => new Promise(resolve => {
      const socket = createConnection(server.path); const lines = []; let buffered = "";
      socket.setEncoding("utf8");
      socket.on("data", chunk => { buffered += chunk; let n; while ((n = buffered.indexOf("\n")) !== -1) { lines.push(JSON.parse(buffered.slice(0, n))); buffered = buffered.slice(n + 1); } });
      socket.once("connect", () => resolve({ socket, lines }));
    });
    const stranger = await open();
    stranger.socket.write(`${JSON.stringify({ type: "hello", token: "guess" })}\n`);
    await until(() => stranger.socket.closed);
    assert.deepEqual(stranger.lines, [{ type: "revoked" }]);
    const owner = await open();
    owner.socket.write(`${JSON.stringify({ type: "hello", token })}\n${JSON.stringify({ id: 1, type: "list" })}\n`);
    await until(() => owner.lines.length === 2);
    assert.deepEqual(owner.lines, [{ type: "ready" }, { id: 1, result: { sessionId: "session-1", type: "list" } }]);
    server.tokens.revoke("session-1");
    await until(() => owner.socket.closed);
    assert.deepEqual(owner.lines.at(-1), { type: "revoked" });
  } finally { await server.close(); }
});

// Computer use (the Cua Driver) reaches a session through its own server and
// follows that session's latest setting (off in Plan mode). The fake driver
// answers a call with the tool's name.
const FAKE_DRIVER = `require("readline").createInterface({ input: process.stdin }).on("line", line => {
  const m = JSON.parse(line); if (m.id == null) return;
  const result = m.method === "initialize" ? { protocolVersion: "2025-06-18", capabilities: { tools: {} }, serverInfo: { name: "cua", version: "1" } }
    : m.method === "tools/list" ? { tools: [{ name: "screenshot", inputSchema: { type: "object" } }] }
    : m.method === "tools/call" ? { content: [{ type: "text", text: "ran " + m.params.name + " on " + process.env.DISPLAY }], isError: false } : {};
  process.stdout.write(JSON.stringify({ jsonrpc: "2.0", id: m.id, result }) + "\\n"); });`;
const DRIVER = { enabled: true, command: process.execPath, args: ["-e", FAKE_DRIVER], env: { DISPLAY: ":1", BAD: 7 } };
const computer = entry => ({ config: { "mcp_servers.cua_driver": entry } });

// Stop must end the computer action, not only Cupola's wait for it: the real
// driver ignores `notifications/cancelled` and keeps acting. This one acts
// (appends to MARK) 300 ms after a call, unless it is stopped first.
const SLOW_DRIVER = `require("readline").createInterface({ input: process.stdin }).on("line", line => {
  const m = JSON.parse(line); if (m.id == null) return;
  const answer = result => process.stdout.write(JSON.stringify({ jsonrpc: "2.0", id: m.id, result }) + "\\n");
  if (m.method !== "tools/call") return answer({});
  setTimeout(() => { require("fs").appendFileSync(process.env.MARK, m.params.arguments.text + "\\n"); answer({ content: [], isError: false }); }, 300); });`;

// The driver's one lifecycle owner, driven through start, Stop, cancel,
// timeout, failed start and replacement orderings across two generations,
// while a second thread's driver acts throughout. This driver takes INIT ms
// to start (failing with FAIL=error, exiting with FAIL=exit), acts after the
// call's `ms` and records NAME, GEN and the text, and its process ID.
const TABLE_DRIVER = `const e = process.env; require("fs").appendFileSync(e.MARK + ".pids", process.pid + "\\n");
require("readline").createInterface({ input: process.stdin }).on("line", line => {
  const m = JSON.parse(line); if (m.id == null) return;
  const answer = (result, error) => process.stdout.write(JSON.stringify({ jsonrpc: "2.0", id: m.id, ...(error ? { error } : { result }) }) + "\\n");
  if (m.method === "initialize") return setTimeout(() => e.FAIL === "exit" ? process.exit(1) : e.FAIL ? answer(null, { code: -32603, message: "no display" })
    : answer({ protocolVersion: m.params.protocolVersion, capabilities: { tools: {} }, serverInfo: { name: "cua", version: "1" } }), +(e.INIT ?? 0));
  if (m.method === "tools/list") return answer({ tools: [{ name: "type_text", inputSchema: { type: "object", properties: { text: { type: "string" }, ms: { type: "number" } } } }] });
  if (m.method !== "tools/call") return answer({});
  setTimeout(() => { require("fs").appendFileSync(e.MARK, e.NAME + e.GEN + ":" + m.params.arguments.text + "\\n");
    answer({ content: [{ type: "text", text: "typed" }], isError: false }); }, m.params.arguments.ms ?? 0); });`;
// Each step is [action, ...]; `gen` sets how thread A's next driver behaves.
const DRIVER_ORDERINGS = [
  { name: "Stop during an action, then a new call", steps: [["gen", 1], ["call", "a1", 300], ["wait", 100], ["stop"], ["gen", 2], ["call", "a2"],
    ["rejects", "a1", /stopped/], ["resolves", "a2"]] },
  { name: "a cancelled call, then a call at once", steps: [["gen", 1], ["call", "a1", 300], ["wait", 100], ["gen", 2], ["cancel", "a1"], ["call", "a2"],
    ["rejects", "a1", /cancelled/], ["resolves", "a2"]] },
  { name: "a timeout, then Stop, then a new call", timeoutMs: 150, steps: [["gen", 1], ["call", "a1", 300], ["rejects", "a1", /in time/], ["stop"],
    ["gen", 2], ["call", "a2"], ["resolves", "a2"]] },
  { name: "Stop while starting, replaced at once", steps: [["gen", 1, { INIT: "200" }], ["call", "a1"], ["wait", 50], ["stop"], ["gen", 2], ["call", "a2"],
    ["rejects", "a1", /stopped/], ["resolves", "a2"]] },
  { name: "a failed start, then a new call", steps: [["gen", 1, { FAIL: "error" }], ["call", "a1"], ["rejects", "a1", /no display/],
    ["gen", 2], ["call", "a2"], ["resolves", "a2"]] },
  { name: "a driver that exits while starting, then a new call", steps: [["gen", 1, { FAIL: "exit" }], ["call", "a1"], ["rejects", "a1", /stopped/],
    ["gen", 2], ["call", "a2"], ["resolves", "a2"]] },
  { name: "Stop while starting, the old start failing after its replacement began", steps: [["gen", 1, { INIT: "150", FAIL: "error" }], ["call", "a1"],
    ["wait", 50], ["stop"], ["gen", 2, { INIT: "250" }], ["call", "a2"], ["rejects", "a1", /stopped/], ["resolves", "a2"]] },
  { name: "Stop while starting, then the stopped call cancelled after its replacement began", steps: [["gen", 1, { INIT: "200" }], ["call", "a1"],
    ["wait", 50], ["stop"], ["gen", 2], ["call", "a2"], ["cancel", "a1"], ["rejects", "a1", /stopped|cancelled/], ["resolves", "a2"]] },
];

for (const row of DRIVER_ORDERINGS) test(`computer-use driver lifecycle: ${row.name}`, { timeout: 10_000 }, async () => {
  const dir = await mkdtemp(join(tmpdir(), "wonder-cua-owner-"));
  const mark = join(dir, "actions");
  const owner = (NAME, timeoutMs) => new CuaDriver({ command: process.execPath, args: ["-e", TABLE_DRIVER], env: { MARK: mark, NAME, GEN: "1" }, timeoutMs });
  const a = owner("A", row.timeoutMs), b = owner("B");
  const calls = new Map(), wait = ms => new Promise(resolve => setTimeout(resolve, ms));
  const call = (id, ms) => {
    const stop = new AbortController(), result = a.call("type_text", { text: id, ms }, stop.signal);
    result.catch(() => {});
    calls.set(id, { stop, result });
  };
  const typed = { content: [{ type: "text", text: "typed" }], isError: false };
  try {
    // Thread B acts throughout thread A's orderings.
    const other = b.call("type_text", { text: "b", ms: 250 }, new AbortController().signal);
    for (const [action, ...args] of row.steps) {
      if (action === "gen") a.env = { MARK: mark, NAME: "A", GEN: String(args[0]), ...args[1] };
      else if (action === "call") call(args[0], args[1]);
      else if (action === "cancel") calls.get(args[0]).stop.abort();
      else if (action === "stop") a.stop();
      else if (action === "wait") await wait(args[0]);
      else if (action === "rejects") await assert.rejects(calls.get(args[0]).result, args[1]);
      else if (action === "resolves") assert.deepEqual(await calls.get(args[0]).result, typed);
    }
    assert.deepEqual(await other, typed, "thread B");
    // Stop still reaches whichever generation is current.
    call("tail", 300); await wait(100); a.stop();
    await assert.rejects(calls.get("tail").result, /stopped/);
    await wait(400);
    // Only the second generation's call and thread B's acted, and no driver
    // process outlived Stop.
    assert.deepEqual((await readFile(mark, "utf8")).split("\n").filter(Boolean).sort(), ["A2:a2", "B1:b"]);
    const alive = pid => { try { process.kill(pid, 0); return true; } catch { return false; } };
    const pids = (await readFile(`${mark}.pids`, "utf8")).split("\n").filter(Boolean).map(Number);
    b.stop(); await wait(200);
    assert.deepEqual(pids.filter(alive), []);
  } finally { a.stop(); b.stop(); await rm(dir, { recursive: true, force: true }); }
});

// Two threads with the same computer-use setting: stopping one thread's
// action leaves the other's running.
test("stopping one thread's computer action leaves another thread's running", { timeout: 10_000 }, async () => {
  const h = await harness();
  const dir = await mkdtemp(join(tmpdir(), "wonder-cua-two-"));
  const mark = join(dir, "actions");
  try {
    const a = (await h.start()).thread, b = (await h.start()).thread;
    const slow = computer({ enabled: true, command: process.execPath, args: ["-e", SLOW_DRIVER], env: { MARK: mark } });
    const [driverA, driverB] = [a, b].map(thread => {
      const session = h.bridge.sessions.get(thread.id);
      Object.assign(session.options, slow);
      return h.bridge.driver(session);
    });
    assert.notEqual(driverA, driverB);
    const stopA = new AbortController();
    const runningA = driverA.call("type_text", { text: "A" }, stopA.signal);
    const runningB = driverB.call("type_text", { text: "B" }, new AbortController().signal);
    await new Promise(resolve => setTimeout(resolve, 100));
    stopA.abort();
    await assert.rejects(runningA, /cancelled/);
    assert.deepEqual(await runningB, { content: [], isError: false });
    await new Promise(resolve => setTimeout(resolve, 400));
    assert.deepEqual(await readFile(mark, "utf8"), "B\n");
  } finally { await h.cleanup(); await rm(dir, { recursive: true, force: true }); }
});

test("computer use is applied and revoked on an existing session", async () => {
  const h = await harness();
  try {
    const { thread } = await h.start([]);
    const own = serverName(thread.id);
    const reply = async entry => {
      const { reply } = await h.ask(thread, { server: own, tool: "screenshot", arguments: {} }, computer(entry));
      return reply;
    };
    const tools = () => h.state("test/state").then(state => state.servers[own] ?? null);
    assert.deepEqual(await reply({ enabled: false }), { missing: true }, "Plan mode");
    assert.equal(await tools(), null);
    assert.deepEqual(await reply(DRIVER), { content: [{ type: "text", text: "ran screenshot on :1" }], isError: false }, "leaving Plan mode");
    // The call above ran during the reply, so it was listed then; once the reply ends the idle thread lists none.
    await until(async () => (await tools())?.length === 0, 3000);
    await reply(DRIVER);
    await reply({ enabled: false });
    await until(async () => await tools() === null, 3000);
    assert.equal((await reply(DRIVER)).isError, false, "a revoked driver comes back");
    // Unchanged turns attach nothing new: leave, enter, leave Plan mode.
    assert.equal((await h.state("test/state")).controls.filter(c => c.method === "session/resume").length, 3);
    // The activity names computer use, not the per-session server.
    const item = h.frames.findLast(f => f.method === "turn/completed").params.turn.items.find(i => i.type === "mcpToolCall");
    assert.deepEqual([item.server, item.tool], ["Computer use", "screenshot"]);
  } finally { await h.cleanup(); }
});

test("two threads in one folder keep their own computer use while both reply", async () => {
  const h = await harness();
  try {
    const a = (await h.start()).thread, b = (await h.start()).thread;
    const servers = async () => Object.fromEntries((await h.state("test/mcp")).servers.filter(s => s.alive).map(s => [s.name, s]));
    // A replies with computer use and stays busy.
    const { turn } = await h.call("turn/start", { threadId: a.id, clientUserMessageId: randomUUID(), input: [{ type: "text", text: "wait" }], ...computer(DRIVER) });
    const acpA = h.bridge.sessions.get(a.id).acpSessionId;
    await until(async () => (await h.state("test/cancelled-sessions")).sessions.includes(acpA));
    const before = (await servers())[serverName(a.id)];
    assert.ok(before.tools.includes("screenshot"));
    // Meanwhile B replies in Plan mode: it cannot reach A's driver, and its
    // own server offers none.
    const borrowed = await h.ask(b, { server: serverName(a.id), tool: "screenshot", arguments: {} }, computer({ enabled: false }));
    assert.equal(borrowed.reply.isError, true);
    assert.match(borrowed.reply.content[0].text, /belongs to another thread/);
    assert.equal((await h.ask(b, { server: "own", tool: "screenshot", arguments: {} }, computer({ enabled: false }))).reply.isError, true);
    const during = await servers();
    assert.equal(during[serverName(b.id)].tools.includes("screenshot"), false);
    // A's server was neither replaced nor stopped.
    assert.equal(during[serverName(a.id)].pid, before.pid);
    assert.ok(during[serverName(a.id)].tools.includes("screenshot"));
    await h.agents.at(-1).request("test/release-cancel", { sessionId: acpA, stopReason: "end_turn" });
    assert.equal((await h.turnDone(turn.id)).status, "completed");
    // The other way round, at the same time.
    const [planA, useB] = await Promise.all([
      h.ask(a, { server: "own", tool: "screenshot", arguments: {} }, computer({ enabled: false })),
      h.ask(b, { server: "own", tool: "screenshot", arguments: {} }, computer(DRIVER)),
    ]);
    assert.equal(planA.reply.isError, true);
    assert.deepEqual(useB.reply, { content: [{ type: "text", text: "ran screenshot on :1" }], isError: false });
    // No approval card was needed for these: the native policy allows them.
    // The daemon is only asked whether the owner has control.
    assert.deepEqual([...new Set(h.frames.filter(f => f.method === "item/tool/call").map(f => f.params.tool))],
      ["wonder_computer_control_check"], "the driver, not the daemon, ran them");
  } finally { await h.cleanup(); }
});

// While the owner controls the computer from a phone the agent's computer use
// waits; Wonder's own tools, such as asking for that takeover, still run.
test("computer use waits while the owner has control, and resumes when it is given back", async () => {
  let held = true, failCheck = false;
  const h = await harness({ answer: params => ({ success: !(failCheck && params.tool === "wonder_computer_control_check"), contentItems: [{ type: "inputText",
    text: params.tool === "wonder_computer_control_check" ? JSON.stringify({ ownerHasControl: held }) : "{\"status\":\"returned\"}" }] }) });
  try {
    const { thread } = await h.start();
    const screenshot = () => h.ask(thread, { server: "own", tool: "screenshot", arguments: {} }, computer(DRIVER)).then(r => r.reply);
    assert.deepEqual(await screenshot(), { content: [{ type: "text", text: "The owner has control; wait." }], isError: true });
    const check = h.frames.findLast(f => f.method === "item/tool/call");
    assert.equal(check.params.tool, "wonder_computer_control_check");
    assert.equal(check.params.threadId, thread.id);
    const takeover = await h.ask(thread, { server: "own", tool: "wonder_request_takeover", arguments: { reason: "Sign in", clientRequestId: randomUUID() } }, computer(DRIVER));
    assert.deepEqual(takeover.reply, { content: [{ type: "text", text: "{\"status\":\"returned\"}" }], isError: false });
    held = false;
    assert.deepEqual(await screenshot(), { content: [{ type: "text", text: "ran screenshot on :1" }], isError: false });
    // Only an explicit "no" lets an action run: a malformed or failed check refuses it.
    const UNKNOWN = { content: [{ type: "text", text: "Cupola couldn't check whether the owner is using this computer, so this action did not run. Try again shortly." }], isError: true };
    held = "unknown";
    assert.deepEqual(await screenshot(), UNKNOWN);
    failCheck = true;
    assert.deepEqual(await screenshot(), UNKNOWN);
  } finally { await h.cleanup(); }
});

// Stop reaches a driver that is still starting or listing its tools: the
// session's owner ends it, as the Claude bridge test of the same name checks.
const STARTING_DRIVER = `require("fs").writeFileSync(process.env.PID, String(process.pid));
require("readline").createInterface({ input: process.stdin }).on("line", line => {
  const m = JSON.parse(line); if (m.id == null) return;
  if (m.method === "initialize" && process.env.HOLD === "initialize") return;
  if (m.method === "tools/list") return; // Never ready.
  process.stdout.write(JSON.stringify({ jsonrpc: "2.0", id: m.id, result: { protocolVersion: "2025-06-18", capabilities: { tools: {} }, serverInfo: { name: "cua", version: "1" } } }) + "\\n"); });`;
for (const hold of ["initialize", "tools/list"]) test(`stopping a reply while its computer-use driver starts ends the driver (held in ${hold})`, { timeout: 10_000 }, async () => {
  const h = await harness();
  const dir = await mkdtemp(join(tmpdir(), "wonder-cua-start-"));
  const pidFile = join(dir, "driver-pid");
  try {
    const { thread } = await h.start([]);
    const { turn } = await h.call("turn/start", { threadId: thread.id, clientUserMessageId: randomUUID(), input: [{ type: "text", text: "wait" }],
      ...computer({ enabled: true, command: process.execPath, args: ["-e", STARTING_DRIVER], env: { PID: pidFile, HOLD: hold } }) });
    let pid;
    while (!(pid = Number(await readFile(pidFile, "utf8").catch(() => "")))) await new Promise(resolve => setTimeout(resolve, 10));
    await h.call("turn/interrupt", { threadId: thread.id, turnId: turn.id });
    const alive = () => { try { process.kill(pid, 0); return true; } catch { return false; } };
    for (let i = 0; alive() && i < 100; i++) await new Promise(resolve => setTimeout(resolve, 10));
    assert.equal(alive(), false, "the starting driver outlived Stop");
    assert.equal((await h.turnDone(turn.id)).status, "interrupted");
  } finally { await h.cleanup(); await rm(dir, { recursive: true, force: true }); }
});


// Contract: the daemon's tool registry and these allowlists name the same tools
// (packages/protocol/thread-tools.json); a tool missing here would vanish from
// OpenCode and Cursor without an error.
test("the Project tool allowlists are packages/protocol/thread-tools.json", () => {
  assert.deepEqual([...PROJECT_TOOLS].sort(), [...NAMES].sort());
  assert.ok(PROJECT_TOOLS.has("wonder_preferences"), "Cupola's own preferences tool, not T3 Code's");
  const server = serverName("session");
  for (const tool of NAMES) {
    assert.deepEqual(wonderTool(qualifiedName(server, tool)), { server, tool });
    assert.deepEqual(wonderTool(`${server}: ${tool}`), { server, tool });
  }
  assert.equal(wonderTool(qualifiedName(server, "wonder_computer_use")), null);
});

// A packaged runtime has no repository around it: it reads the copy packaging
// puts beside thread-tools.mjs, and refuses to start without one. The Claude
// runtime ships the same loader.
test("a packaged runtime reads its own copy of the tool list and needs one", async t => {
  const root = await mkdtemp(join(tmpdir(), "wonder-acp-package-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const runtime = join(root, "Contents", "Resources", "acp-runtime");
  await mkdir(runtime, { recursive: true });
  const base = pathToFileURL(join(runtime, "thread-tools.mjs"));
  assert.throws(() => loadProjectTools(base), /thread-tools\.json is missing/);
  await copyFile(new URL("../../../packages/protocol/thread-tools.json", import.meta.url), join(runtime, "thread-tools.json"));
  assert.deepEqual([...loadProjectTools(base)].sort(), [...NAMES].sort());
  await writeFile(join(runtime, "thread-tools.json"), JSON.stringify({ names: [] }));
  assert.throws(() => loadProjectTools(base), /does not list/);
  assert.equal(await readFile(new URL("../thread-tools.mjs", import.meta.url), "utf8"),
    await readFile(new URL("../../claude-runtime/thread-tools.mjs", import.meta.url), "utf8"),
    "Keep services/claude-runtime/thread-tools.mjs identical to this runtime's loader.");
});

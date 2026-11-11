// A scripted ACP agent for tests: no model work, deterministic updates.
import { createInterface } from "node:readline";
import { closeSync, renameSync, writeFileSync } from "node:fs";
import { spawn } from "node:child_process";
const write = frame => process.stdout.write(`${JSON.stringify({ jsonrpc: "2.0", ...frame })}\n`);
const update = (sessionId, update) => write({ method: "session/update", params: { sessionId, update } });
const cursor = process.env.WONDER_FAKE_ACP_PROVIDER === "cursor";
let authenticated = !cursor;
const cursorModel = "gpt-5.6-luna[context=272k,reasoning=medium,fast=false]";
let nextId = 1000, sessions = 0;
const states = new Map(), prompts = [], configs = [], controls = [];
const commandScript = JSON.parse(process.env.WONDER_FAKE_ACP_COMMANDS ?? "[]");
const permissionAnswers = [];
const permissionOptions = JSON.parse(process.env.WONDER_FAKE_ACP_PERMISSION_OPTIONS ?? "null");
const setups = new Map();
const streams = new Map();
function setup(id, method, sessionId) {
  const script = commandScript.shift() ?? {};
  const sendCommands = rows => {
    for (const row of rows ?? []) update(row.sessionId ?? sessionId, { sessionUpdate: "available_commands_update", availableCommands: row.commands });
  };
  sendCommands(script.before);
  const finish = fail => {
    setups.delete(sessionId);
    if (fail) write({ id, error: { code: -32000, message: "Synthetic setup refused" } });
    else write({ id, result: { ...(method === "session/new" ? { sessionId } : {}), ...configuration(sessionId) } });
    sendCommands(script.after);
  };
  if (script.hold) setups.set(sessionId, finish);
  else finish(script.fail);
}
const modelId = () => fault === "custom-config" ? "provider-model" : "model";
const modeId = () => fault === "custom-config" ? "agent-mode" : "mode";
const state = sessionId => {
  if (!states.has(sessionId)) states.set(sessionId, { model: cursor ? "default[]" : "opencode/other-model", mode: cursor ? "agent" : "build" });
  return states.get(sessionId);
};
const configuration = sessionId => {
  const current = state(sessionId);
  if (cursor) return { configOptions: [
    { id: "model", category: "model", type: "select", currentValue: current.model, options: [{ value: "default[]", name: "Auto" }, { value: cursorModel, name: "GPT-5.6 Luna" }] },
    { id: "mode", category: "mode", type: "select", currentValue: current.mode, options: [{ value: "agent", name: "Agent" }, { value: "plan", name: "Plan" }, { value: "ask", name: "Ask" }] },
  ] };
  if (fault === "legacy-modes") return { models: { currentModelId: "opencode/free-model" }, modes: { currentModeId: current.mode, availableModes: [{ id: "build" }, { id: "plan" }] } };
  return { configOptions: [
    { id: modelId(), category: "model", type: "select", currentValue: current.model, options: [{ group: "free", options: [{ value: "opencode/free-model", name: "Free" }, { value: "opencode/other-model", name: "Other" }] }] },
    { id: modeId(), category: "mode", type: "select", currentValue: current.mode, options: [{ value: "build", name: "Build" }, { value: "plan", name: "Plan" }] },
  ] };
};
const waiting = new Map(), cancels = new Map();
const ask = (method, params) => new Promise(resolve => { const id = nextId++; waiting.set(id, resolve); write({ id, method, params }); });
// Native policy is fixture input, independent of Cupola's request fields.
const permission = process.env.WONDER_FAKE_NATIVE_PERMISSION ?? "allow";
const fault = process.env.WONDER_FAKE_ACP_FAULT;
if (["ignore-close", "no-initialize", "lost-transport"].includes(fault)) {
  process.on("SIGTERM", () => {});
  setInterval(() => {}, 1000);
}
// Like OpenCode, MCP servers are registered for the whole process by name: a
// session sees every registered server, and a new config replaces the old.
const servers = new Map(), sessionServers = new Map();
function mcpClient(server) {
  const child = spawn(server.command, server.args ?? [], { env: { ...process.env, ...Object.fromEntries((server.env ?? []).map(e => [e.name, e.value])) }, stdio: ["pipe", "pipe", "inherit"] });
  const client = { name: server.name, key: JSON.stringify(server), child, alive: true, tools: [], pending: new Map(), nextId: 1, lists: 0 };
  const exited = () => { client.alive = false; if (servers.get(server.name) === client) servers.delete(server.name); for (const reject of client.pending.values()) reject.reject(new Error("closed")); };
  child.on("exit", exited);
  child.on("error", exited);
  const rpc = (method, params) => new Promise((resolve, reject) => {
    const id = client.nextId++; client.pending.set(id, { resolve, reject });
    child.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", id, method, params })}\n`);
  });
  // Like OpenCode, a tool call needs the tool in the list the client last read; a re-list after list_changed is slow.
  const list = async () => { if (client.lists > 0 && process.env.WONDER_FAKE_ACP_RELIST_MS) await new Promise(r => setTimeout(r, Number(process.env.WONDER_FAKE_ACP_RELIST_MS))); client.tools = (await rpc("tools/list", {})).tools; client.lists++; };
  client.call = (name, args) => process.env.WONDER_FAKE_ACP_STRICT_LIST && !client.tools.some(t => t.name === name)
    ? Promise.resolve({ isError: true, content: [{ type: "text", text: "tool not listed" }] }) : rpc("tools/call", { name, arguments: args });
  (async () => {
    for await (const line of createInterface({ input: child.stdout })) {
      const frame = JSON.parse(line);
      if (frame.method === "notifications/tools/list_changed") { list().catch(() => {}); continue; }
      const waiter = client.pending.get(frame.id);
      if (!waiter) continue;
      client.pending.delete(frame.id);
      if (frame.error) waiter.reject(Object.assign(new Error(frame.error.message), { code: frame.error.code })); else waiter.resolve(frame.result);
    }
  })();
  client.ready = (async () => {
    const init = await rpc("initialize", { protocolVersion: "2025-06-18", capabilities: {}, clientInfo: { name: "fake", version: "1" } });
    client.capabilities = init.capabilities;
    child.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", method: "notifications/initialized" })}\n`);
    await list();
  })().catch(() => {});
  return client;
}
// Like OpenCode, a session skips a config it registered before (even after
// another config replaced it) and keeps a server that a later attach omits.
const registered = new Map();
async function register(sessionId, mcpServers = []) {
  if (mcpServers.length) sessionServers.set(sessionId, mcpServers[0].name);
  for (const server of mcpServers) {
    if (!registered.has(sessionId)) registered.set(sessionId, new Set());
    if (registered.get(sessionId).has(JSON.stringify(server))) continue;
    registered.get(sessionId).add(JSON.stringify(server));
    const current = servers.get(server.name);
    if (current?.key === JSON.stringify(server)) continue;
    current?.child.kill();
    const client = mcpClient(server);
    servers.set(server.name, client);
    await client.ready;
  }
}
// How the agent reports an MCP call: OpenCode titles it `<server>_<tool>`
// with the arguments as its input; Cursor (2026.10.19) titles it
// `<server>: <tool>` and wraps the arguments with the server and tool.
const cursorStyle = process.env.WONDER_FAKE_TOOL_STYLE === "cursor";
const qualified = (server, tool) => cursorStyle ? `${server}: ${tool}` : `${server.replace(/[^a-zA-Z0-9_-]/g, "_")}_${tool}`;
const reported = (server, tool, args) => cursorStyle ? { providerIdentifier: server, toolName: tool, args } : args;
async function toolCall(id, sessionId, request) {
  const name = request.server === "own" ? sessionServers.get(sessionId) : request.server;
  const client = servers.get(name);
  const toolCallId = `tool-${nextId++}`;
  if (request.report !== false) {
    update(sessionId, { sessionUpdate: "tool_call", toolCallId, title: qualified(name, request.tool), kind: "other", status: "pending", rawInput: {} });
    update(sessionId, { sessionUpdate: "tool_call_update", toolCallId, title: qualified(name, request.tool), kind: "other", status: "in_progress", rawInput: reported(name, request.tool, request.arguments) });
  }
  if (permission === "ask") {
    const answer = await ask("session/request_permission", { sessionId, toolCall: { toolCallId, title: qualified(name, request.tool), kind: "other", rawInput: reported(name, request.tool, request.arguments) },
      options: [{ optionId: "once", kind: "allow_once", name: "Allow once" }, { optionId: "reject", kind: "reject_once", name: "Reject" }] });
    if (answer.outcome?.optionId !== "once") {
      update(sessionId, { sessionUpdate: "tool_call_update", toolCallId, status: "failed", content: [{ type: "content", content: { type: "text", text: "Rejected" } }] });
      update(sessionId, { sessionUpdate: "agent_message_chunk", content: { type: "text", text: JSON.stringify({ rejected: true }) } });
      return write({ id, result: { stopReason: "end_turn" } });
    }
  }
  let result;
  try { result = client?.alive ? await client.call(request.tool, request.arguments) : { missing: true }; }
  catch (error) { result = { error: error.message }; }
  update(sessionId, { sessionUpdate: "tool_call_update", toolCallId, status: result.isError === false ? "completed" : "failed",
    content: (result.content ?? []).map(content => ({ type: "content", content })) });
  update(sessionId, { sessionUpdate: "agent_message_chunk", content: { type: "text", text: JSON.stringify(result) } });
  write({ id, result: { stopReason: "end_turn" } });
}
const pairedTodos = new Set();
let pairedReady = null;
async function prompt(id, { sessionId, prompt }) {
  prompts.push({ sessionId, requestId: id, ...state(sessionId) });
  const text = prompt.map(p => p.text ?? "").join(" ");
  if (text.startsWith("burst ")) {
    const { count, size, hold = false, fail = false } = JSON.parse(text.slice(6));
    let cancelled = false;
    cancels.set(sessionId, () => { cancelled = true; });
    streams.set(sessionId, 0);
    for (let index = 0; index < count && !cancelled; index++) {
      const delta = `${index}:` + "é".repeat(size);
      // Honor the native pipe too: this fixture must not manufacture its own
      // hidden queue while measuring the daemon-facing queue.
      await new Promise(resolve => process.stdout.write(JSON.stringify({ jsonrpc: "2.0", method: "session/update",
        params: { sessionId, update: { sessionUpdate: "agent_message_chunk", content: { type: "text", text: delta } } } }) + "\n", resolve));
      streams.set(sessionId, index + 1);
      if (process.env.WONDER_FAKE_ACP_PROGRESS && (index % 128 === 0 || index + 1 === count)) {
        const path = process.env.WONDER_FAKE_ACP_PROGRESS;
        writeFileSync(`${path}.tmp`, JSON.stringify({ sessionId, count: index + 1 }));
        // Windows refuses to replace a file the test is reading (EPERM);
        // retry instead of ending the burst early.
        for (let attempt = 0; ; attempt++) {
          try { renameSync(`${path}.tmp`, path); break; }
          catch (error) {
            if (attempt >= 200 || !["EPERM", "EBUSY", "EACCES"].includes(error.code)) throw error;
            Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 5);
          }
        }
      }
    }
    if (hold && !cancelled) await new Promise(resolve => cancels.set(sessionId, () => { cancelled = true; resolve(); }));
    return fail && !cancelled ? write({ id, error: { code: -32000, message: "Synthetic burst failed" } })
      : write({ id, result: { stopReason: cancelled ? "cancelled" : "end_turn" } });
  }
  if (fault === "invalid-completion") return write({ id, result: {} });
  if (fault === "lost-transport") { process.stdout.write("{broken\n"); return; }
  if (text.startsWith("call ")) return toolCall(id, sessionId, JSON.parse(text.slice(5)));
  // Cursor's to-do extension: a full list naming no session, then a merge.
  if (text === "cursor todos") {
    write({ method: "cursor/update_todos", params: { toolCallId: "todo", merge: false, todos: [
      { id: "1", content: "Read the parser", status: "in_progress" }, { id: "2", content: "Fix the bug", status: "pending" }] } });
    write({ method: "cursor/update_todos", params: { sessionId, toolCallId: "todo", merge: true, todos: [
      { id: "1", content: "Read the parser", status: "completed" }] } });
    return write({ id, result: { stopReason: "end_turn" } });
  }
  // Two replies at once, each sending a to-do list that names no session
  // before the tool call that made it, then a merge naming none either.
  if (text.startsWith("paired todos ")) {
    const name = text.slice(13);
    // Both replies are in progress before either sends anything.
    pairedTodos.add(sessionId);
    if (pairedTodos.size < 2) await new Promise(resolve => { pairedReady = resolve; });
    else pairedReady?.();
    const tool = `todo-${name}`;
    write({ method: "cursor/update_todos", params: { toolCallId: tool, merge: false, todos: [
      { id: "1", content: `${name} first`, status: "in_progress" }, { id: "2", content: `${name} second`, status: "pending" }] } });
    update(sessionId, { sessionUpdate: "tool_call", toolCallId: tool, kind: "other", title: "Update todos", status: "completed", rawInput: {} });
    write({ method: "cursor/update_todos", params: { toolCallId: tool, merge: true, todos: [{ id: "1", content: `${name} first`, status: "completed" }] } });
    return write({ id, result: { stopReason: "end_turn" } });
  }
  if (text === "tool images") {
    for (const kind of ["read", "execute", "edit"]) {
      update(sessionId, { sessionUpdate: "tool_call", toolCallId: `image-${kind}`, kind, title: kind, status: "in_progress" });
      update(sessionId, { sessionUpdate: "tool_call_update", toolCallId: `image-${kind}`, status: kind === "edit" ? "failed" : "completed",
        content: [{ type: "content", content: { type: "text", text: "Preview" } },
          { type: "content", content: { type: "image", mimeType: "image/png", data: "aW1hZ2UgYnl0ZXM=" } }],
        rawOutput: { output: "Preview", attachments: [{ mime: "image/png", url: "data:image/png;base64,aW1hZ2UgYnl0ZXM=" }] } });
    }
    return write({ id, result: { stopReason: "end_turn" } });
  }
  if (text.includes("wait")) {
    update(sessionId, { sessionUpdate: "agent_message_chunk", content: { type: "text", text: "Working" } });
    if (fault === "close-input-on-prompt") { process.stdin.destroy(); closeSync(0); setInterval(() => {}, 1000); }
    const releasedReason = await new Promise(resolve => cancels.set(sessionId, resolve));
    if (fault === "held-cancel") update(sessionId, { sessionUpdate: "agent_message_chunk", content: { type: "text", text: " — stopped" } });
    if (fault === "failure-during-stop") return write({ id, error: { code: -32000, message: "Native prompt failed during Stop." } });
    return write({ id, result: { stopReason: releasedReason ?? (fault === "completion-during-stop" ? "end_turn" : "cancelled") } });
  }
  if (text.includes("command")) {
    update(sessionId, { sessionUpdate: "agent_message_chunk", content: { type: "text", text: "Checking." } });
    update(sessionId, { sessionUpdate: "tool_call", toolCallId: "call-1", title: "bash", kind: "execute", status: "pending", rawInput: {} });
    update(sessionId, { sessionUpdate: "tool_call_update", toolCallId: "call-1", status: "in_progress", kind: "execute", title: "ls", rawInput: { command: "ls", workdir: "/tmp" } });
    if (permission === "ask") {
      const answer = await ask("session/request_permission", { sessionId, toolCall: { toolCallId: "call-1", title: "ls", kind: "execute", rawInput: { command: "ls" } },
        options: permissionOptions ?? [{ optionId: "once", kind: "allow_once", name: "Allow once" }, { optionId: "always", kind: "allow_always", name: "Always" }, { optionId: "reject", kind: "reject_once", name: "Reject" }] });
      permissionAnswers.push(answer.outcome);
      const selected = (permissionOptions ?? [{ optionId: "once", kind: "allow_once" }, { optionId: "always", kind: "allow_always" }]).find(option => option.optionId === answer.outcome?.optionId);
      if (!["allow_once", "allow_always"].includes(selected?.kind)) {
        update(sessionId, { sessionUpdate: "tool_call_update", toolCallId: "call-1", status: "failed", content: [{ type: "content", content: { type: "text", text: "Rejected" } }] });
        return write({ id, result: { stopReason: "end_turn" } });
      }
    }
    update(sessionId, { sessionUpdate: "tool_call_update", toolCallId: "call-1", status: "completed", title: "ls", rawOutput: { output: "a.txt\n", metadata: { exit: 0 } } });
    update(sessionId, { sessionUpdate: "plan", entries: [{ content: "List files", status: "completed", priority: "medium" }] });
    update(sessionId, { sessionUpdate: "agent_message_chunk", content: { type: "text", text: "Done." } });
    return write({ id, result: { stopReason: "end_turn" } });
  }
  if (text.includes("edit")) {
    update(sessionId, { sessionUpdate: "tool_call", toolCallId: "call-2", title: "edit", kind: "edit", status: "pending", rawInput: {} });
    update(sessionId, { sessionUpdate: "tool_call_update", toolCallId: "call-2", status: "completed", title: "/tmp/a.txt",
      content: [{ type: "content", content: { type: "text", text: "Edit applied successfully." } }, { type: "diff", path: "/tmp/a.txt", oldText: "hi", newText: "bye" }] });
    return write({ id, result: { stopReason: "end_turn" } });
  }
  update(sessionId, { sessionUpdate: "agent_thought_chunk", content: { type: "text", text: "Hmm" } });
  for (const part of ["Hello", " there"]) update(sessionId, { sessionUpdate: "agent_message_chunk", content: { type: "text", text: part } });
  update(sessionId, { sessionUpdate: "usage_update", used: 1200, size: 200000 });
  write({ id, result: { stopReason: "end_turn" } });
}
for await (const line of createInterface({ input: process.stdin })) {
  if (!line.trim()) continue;
  const frame = JSON.parse(line);
  if (frame.id != null && !frame.method) { waiting.get(frame.id)?.(frame.result ?? {}); waiting.delete(frame.id); continue; }
  const { id, method, params } = frame;
  if (method.startsWith("session/") || method === "authenticate") {
    controls.push({ method, sessionId: params.sessionId, ...(params.mcpServers ? { mcpServers: params.mcpServers.map(s => s.name) } : {}) });
    if (process.env.WONDER_FAKE_ACP_CONTROLS) writeFileSync(process.env.WONDER_FAKE_ACP_CONTROLS, JSON.stringify(controls));
  }
  if (method === "initialize" && fault === "reject-initialize") write({ id, error: { code: -32000, message: "Synthetic startup refused" } });
  else if (method === "initialize" && fault !== "no-initialize") write({ id, result: { protocolVersion: 1, ...(cursor ? { authMethods: [{ id: "cursor_login" }] } : {}), agentCapabilities: { loadSession: true, promptCapabilities: { image: true }, sessionCapabilities: { list: {}, ...((cursor || fault === "load-only") ? {} : { resume: {} }) } } } });
  else if (method === "initialize") continue;
  else if (method === "authenticate") { authenticated = params.methodId === "cursor_login"; write({ id, result: {} }); }
  else if (cursor && method.startsWith("session/") && !authenticated) write({ id, error: { code: -32000, message: "Authenticate first" } });
  else if (method === "test/no-reply") continue;
  else if (method === "test/delay") setTimeout(() => write({ id, result: { echoed: params.text } }), params.delayMs);
  else if (method === "test/pause-input") {
    write({ id, result: {} }); process.stdin.pause(); setInterval(() => {}, 1000);
    await new Promise(() => {});
  }
  else if (method === "test/malformed") process.stdout.write("{broken\n");
  else if (method === "test/null") process.stdout.write("null\n");
  else if (method === "test/invalid-response") write({ id, error: null });
  else if (method === "test/oversized") process.stdout.write(JSON.stringify({ jsonrpc: "2.0", id, result: "é".repeat(17 * 1024 * 1024) }) + "\n");
  else if (method === "test/cancelled-sessions") write({ id, result: { sessions: [...cancels.keys()] } });
  else if (method === "test/release-cancel") { cancels.get(params.sessionId)?.(params.stopReason); write({ id, result: {} }); }
  else if (method === "test/release-config") { waiting.get("config")?.(); waiting.delete("config"); write({ id, result: {} }); }
  else if (method === "test/change-mode") {
    state(params.sessionId).mode = params.mode;
    update(params.sessionId, { sessionUpdate: "config_option_update", ...configuration(params.sessionId) });
    write({ id, result: {} });
  }
  else if (method === "test/commands") {
    update(params.sessionId, { sessionUpdate: "available_commands_update", availableCommands: params.commands });
    write({ id, result: {} });
  }
  else if (method === "test/release-setup") { setups.get(params.sessionId)?.(params.fail); write({ id, result: {} }); }
  else if (method === "test/mcp") write({ id, result: { servers: [...servers.values()].map(c => ({ name: c.name, alive: c.alive, tools: c.tools.map(t => t.name), lists: c.lists, listChanged: c.capabilities?.tools?.listChanged === true, pid: c.child.pid })) } });
  else if (method === "test/mcp-call") {
    const client = servers.get(params.server);
    if (!client?.alive) write({ id, result: { missing: true } });
    else client.call(params.tool, params.arguments ?? {}).then(result => write({ id, result }), error => write({ id, result: { error: error.message } }));
  }
  else if (method === "test/state") write({ id, result: { sessions, prompts, configs, controls, permissionAnswers, streams: Object.fromEntries(streams), setups: [...setups.keys()],
    servers: Object.fromEntries([...servers].filter(([, client]) => client.alive).map(([name, client]) => [name, client.tools.map(tool => tool.name)])) } });
  else if (method === "session/new") {
    sessions++;
    const sessionId = commandScript[0]?.sessionId ?? `ses_${sessions}_${process.pid}`;
    await register(sessionId, params.mcpServers);
    if (fault === "reject-create") write({ id, error: { code: -32000, message: "Synthetic startup refused" } });
    else if (fault !== "lost-create") setup(id, method, sessionId);
  }
  else if (["session/resume", "session/load"].includes(method)) { await register(params.sessionId, params.mcpServers); setup(id, method, params.sessionId); }
  else if (method === "session/set_config_option") {
    configs.push(params);
    if (params.configId === modeId() && params.value === "build" && fault === "reject-build") write({ id, error: { code: -32000, message: "Cannot leave plan mode" } });
    else if (params.configId !== modelId() && params.configId !== modeId()) write({ id, error: { code: -32602, message: "Unknown config ID" } });
    else {
      if (fault === "held-config" && params.configId === modelId()) {
        new Promise(resolve => waiting.set("config", resolve)).then(() => {
          state(params.sessionId).model = params.value;
          write({ id, result: configuration(params.sessionId) });
        });
        continue;
      }
      if (fault !== "unconfirmed-config") state(params.sessionId)[params.configId === modelId() ? "model" : "mode"] = params.value;
      write({ id, result: configuration(params.sessionId) });
    }
  }
  else if (method === "session/set_mode") {
    configs.push(params); state(params.sessionId).mode = params.modeId; write({ id, result: {} });
  }
  else if (method === "session/prompt") prompt(id, params);
  else if (method === "session/cancel") { if (!["ignore-cancel", "held-cancel"].includes(fault)) cancels.get(params.sessionId)?.(); if (fault !== "held-cancel") cancels.set(params.sessionId, () => {}); }
  else if (id != null) write({ id, error: { code: -32601, message: "unknown" } });
}
// A closed client ends the agent and, with it, the MCP servers it started.
for (const client of servers.values()) client.child.kill();

import { test } from "node:test";
import assert from "node:assert/strict";
import { createServer } from "node:http";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { createInterface } from "node:readline";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

// Windows: child.kill(signal) is TerminateProcess, so no runtime ever sees a
// SIGTERM there; wonderd ends it by closing stdin (the EOF case).
const NO_SIGTERM = process.platform === "win32" && "Windows has no SIGTERM delivery; child.kill terminates the process outright";

// Exercise daemon-facing pipes with a scripted CLI only, never a provider.
async function runtime(t, { permission, provider = "opencode", endpoint } = {}) {
  const root = await mkdtemp(join(tmpdir(), "wonder-acp-pipes-"));
  const binary = join(root, provider === "cursor" ? "agent" : "opencode");
  await writeFile(binary, `#!${process.execPath}
if (process.argv[2] === "--version") console.log("2.2.5");
else if (process.argv[2] === "status") console.log(JSON.stringify({ isAuthenticated: true }));
else if (process.argv[2] === "debug") console.log("[]");
else if (process.argv[2] === "api") console.log(JSON.stringify({ data: Array.from({ length: 100 }, (_, i) => ({ providerID: "local", id: "model" + i, name: "Model".repeat(200) })) }));
else if (process.argv[2] === "models") {
  for (let i = 0; i < 100; i++) { console.log("local/model" + i); console.log(JSON.stringify({ name: "Model".repeat(200) }, null, 2)); }
} else if (process.argv[2] === "acp") {
  process.env.WONDER_FAKE_ACP_PROVIDER = ${JSON.stringify(provider)};
  if (process.env.FIXTURE_BASE_URL) {
    const response = await fetch(process.env.FIXTURE_BASE_URL + '/${provider}', { headers: { Authorization: 'Bearer ' + process.env.FIXTURE_API_KEY } });
    if (!response.ok) throw new Error('Fixture endpoint refused');
  }
  const { writeFileSync } = await import("node:fs");
  writeFileSync(${JSON.stringify(join(root, "permission.json"))}, JSON.stringify({ permission: process.env.OPENCODE_PERMISSION ?? null }));
  await import(${JSON.stringify(new URL("./fake-agent.mjs", import.meta.url).href)});
} else process.exit(2);
`, { mode: 0o700 });
  const child = spawn(process.execPath, [fileURLToPath(new URL("../main.mjs", import.meta.url)), "--state-dir", root, "--provider", provider], {
    env: { ...process.env, WONDER_OPENCODE_BIN: binary, WONDER_CURSOR_BIN: binary, FIXTURE_BASE_URL: endpoint, FIXTURE_API_KEY: endpoint ? "fixture-only" : undefined, OPENCODE_PERMISSION: permission, WONDER_FAKE_ACP_PROGRESS: join(root, "progress.json") }, stdio: ["pipe", "pipe", "pipe"],
  });
  const exited = once(child, "exit");
  child.stdin.on("error", () => {});
  let stderr = "";
  child.stderr.setEncoding("utf8").on("data", text => { stderr += text; });
  t.after(async () => {
    if (child.exitCode == null && child.signalCode == null) { child.kill("SIGKILL"); await exited; }
    await rm(root, { recursive: true, force: true });
  });
  return { root, child, exited, stderr: () => stderr };
}

test("the bridge flushes ordinary replies before stdin closes", { timeout: 10_000 }, async t => {
  const { child, exited, stderr } = await runtime(t);
  let stdout = "";
  child.stdout.setEncoding("utf8").on("data", text => { stdout += text; });
  child.stdin.end(JSON.stringify({ id: 1, method: "initialize" }) + "\n");
  assert.deepEqual(await exited, [0, null], stderr());
  assert.equal(JSON.parse(stdout).result.wonderBridge.family, "opencode");
});

test("a daemon that stops reading cannot hold shutdown open", { timeout: 15_000 }, async t => {
  const { child, exited } = await runtime(t);
  // Read the catalog once first: shutdown cancels a check still running, so
  // the replies below come from the cached catalog rather than a new check.
  child.stdin.write(JSON.stringify({ id: "primed", method: "model/list" }) + "\n");
  await new Promise(resolve => {
    let text = "";
    child.stdout.setEncoding("utf8").on("data", function primed(chunk) {
      text += chunk;
      if (text.includes("\n")) { child.stdout.off("data", primed); child.stdout.pause(); resolve(); }
    });
  });
  // More than the pipe can hold, below the request admission and memory caps.
  child.stdin.end(Array.from({ length: 100 }, (_, id) => JSON.stringify({ id, method: "model/list" }) + "\n").join(""));
  const started = performance.now();
  const [code, signal] = await exited;
  assert.equal(code, 1);
  assert.equal(signal, null);
  assert.ok(performance.now() - started < 12_000);
});

test("a closed output pipe exits without waiting for another input", { timeout: 10_000 }, async t => {
  const { child, exited } = await runtime(t);
  child.stdout.destroy();
  child.stdin.write(JSON.stringify({ id: 1, method: "initialize" }) + "\n");
  assert.deepEqual(await exited, [1, null]);
});

// The launcher must preserve native permissions exactly, including no env
// override, rather than manufacture a global policy with different precedence.
for (const permission of [undefined, JSON.stringify({ task: "ask", bash: "deny" })]) {
  test(`the native permission environment is preserved when ${permission === undefined ? "absent" : "configured"}`, { timeout: 10_000 }, async t => {
    const { root, child, exited, stderr } = await runtime(t, { permission });
    // Wait on the reply itself; the test timeout is the only hang guard. A
    // separate wall-clock deadline judged on a timer fails whenever the Mac
    // stalls the processes for longer than it, even though the reply arrives.
    let stdout = "";
    const waiting = new Map();
    child.stdout.setEncoding("utf8").on("data", text => {
      stdout += text;
      for (const [id, resolve] of waiting) if (stdout.includes(`"id":${id},`)) { waiting.delete(id); resolve(); }
    });
    const received = id => stdout.includes(`"id":${id},`) ? Promise.resolve() : new Promise(resolve => waiting.set(id, resolve));
    // Starting a thread must discover the installed CLI itself, even before
    // an account/model request has primed the catalog.
    child.stdin.write(JSON.stringify({ id: 2, method: "thread/start", params: {
      requestId: "native-policy", cwd: root, model: "opencode:local/model0",
    } }) + "\n");
    await received(2);
    const reply = stdout.trim().split("\n").map(line => JSON.parse(line)).find(frame => frame.id === 2);
    assert.ok(reply.result?.thread.sessionId, JSON.stringify(reply));
    assert.deepEqual(JSON.parse(await readFile(join(root, "permission.json"), "utf8")), { permission: permission ?? null });
    child.stdin.end();
    assert.deepEqual(await exited, [0, null], stderr());
  });
}


async function until(check, ms = 10_000) {
  const deadline = performance.now() + ms;
  while (!(await check())) {
    if (performance.now() >= deadline) throw new Error("runtime test deadline exceeded");
    await new Promise(resolve => setTimeout(resolve, 5));
  }
}

// Keep the data listener installed while paused: a readable listener or its
// removal can resume a pipe and invalidate the backpressure reproduction.
function conversation(child, stderr) {
  let buffered = "", nextId = 100;
  const frames = [];
  child.stdout.setEncoding("utf8").on("data", text => {
    buffered += text;
    let newline;
    while ((newline = buffered.indexOf("\n")) !== -1) {
      frames.push(JSON.parse(buffered.slice(0, newline)));
      buffered = buffered.slice(newline + 1);
    }
  });
  const send = (method, params) => {
    const id = nextId++;
    child.stdin.write(JSON.stringify({ id, method, params }) + "\n");
    return id;
  };
  const call = async (method, params) => {
    const id = send(method, params);
    await until(() => frames.some(f => f.id === id));
    const reply = frames.find(f => f.id === id);
    assert.ok(reply.result, JSON.stringify(reply) + stderr());
    return reply.result;
  };
  return { frames, send, call };
}

async function progress(root) {
  return JSON.parse(await readFile(join(root, "progress.json"), "utf8").catch(() => "{}"));
}

async function storedTurn(root, thread) {
  return JSON.parse(await readFile(join(root, "sessions", `${thread.id}.json`), "utf8")).turns[0];
}

test("real streamed deltas below the shared output bound drain after stdout resumes", { timeout: 10_000 }, async t => {
  const { root, child, exited, stderr } = await runtime(t);
  const rpc = conversation(child, stderr);
  const { thread } = await rpc.call("thread/start", { requestId: "burst-drain", cwd: root, model: "opencode:opencode/free-model" });
  const beforeFrames = rpc.frames.length;
  child.stdout.pause();
  let resumed = false;
  child.stdout.once("resume", () => { resumed = true; });
  const receiptId = rpc.send("turn/start", { threadId: thread.id, clientUserMessageId: "burst-drain",
    input: [{ type: "text", text: 'burst {"count":100,"size":4096}' }] });
  await until(async () => (await progress(root)).count === 100 && (await storedTurn(root, thread))?.status === "completed");
  assert.equal(child.stdout.isPaused(), true);
  assert.equal(resumed, false);
  assert.equal(rpc.frames.some(f => f.id === receiptId), false);
  assert.equal(child.exitCode, null);
  assert.equal(rpc.frames.length, beforeFrames);
  t.diagnostic(JSON.stringify({ phase: "paused", nativeDeltas: 100, bufferedBytes: child.stdout.readableLength, deliveredSincePause: rpc.frames.length - beforeFrames, isPaused: child.stdout.isPaused() }));
  child.stdout.resume();
  await until(() => rpc.frames.some(f => f.method === "turn/completed"));
  const receipt = rpc.frames.findIndex(f => f.id === receiptId);
  const events = rpc.frames.filter(f => f.params?.turnId);
  assert.ok(receipt >= 0 && receipt < rpc.frames.indexOf(events[0]));
  assert.deepEqual(events.map(f => f.method), ["turn/started", "item/started", ...Array(100).fill("item/agentMessage/delta"), "item/completed", "turn/completed"]);
  const expected = Array.from({ length: 100 }, (_, i) => `${i}:` + "é".repeat(4096));
  assert.deepEqual(events.filter(f => f.method === "item/agentMessage/delta").map(f => f.params.delta), expected);
  assert.equal(events.at(-1).params.turn.status, "completed");
  assert.equal(events.at(-1).params.turn.items[0].text, expected.join(""));
  child.stdin.end();
  assert.deepEqual(await exited, [0, null], stderr());
  t.diagnostic(JSON.stringify({ phase: "drained", outputEvents: events.length, terminal: events.at(-1).params.turn.status, exit: child.exitCode }));
});

for (const [kind, count, size] of [["bytes", 2500, 8192], ["items", 5000, 1]]) {
  test(`real streamed deltas hit the shared ${kind} bound while stdout stays paused`, { timeout: 12_000 }, async t => {
    const { root, child, exited, stderr } = await runtime(t);
    const rpc = conversation(child, stderr);
    const first = (await rpc.call("thread/start", { requestId: `overflow-${kind}`, cwd: root, model: "opencode:opencode/free-model" })).thread;
    const second = (await rpc.call("thread/start", { requestId: `peer-${kind}`, cwd: root, model: "opencode:opencode/free-model" })).thread;
    await rpc.call("turn/start", { threadId: second.id, input: [{ type: "text", text: "wait" }] });
    await until(() => rpc.frames.some(f => f.method === "item/agentMessage/delta"));
    child.stdout.pause();
    let resumed = false;
    child.stdout.once("resume", () => { resumed = true; });
    const beforeFrames = rpc.frames.length;
    const started = performance.now();
    rpc.send("turn/start", { threadId: first.id, clientUserMessageId: `overflow-${kind}`,
      input: [{ type: "text", text: `burst ${JSON.stringify({ count, size, hold: true })}` }] });
    // The provider's progress file is independent of daemon stdout. On the
    // baseline it proves the whole burst escaped the writer's byte accounting.
    // Wait for the transport decision itself, rather than race the provider's
    // final write callback against the runtime reading its remaining ACP data.
    await until(() => /output exceeded/.test(stderr()), 3000).catch(() => {});
    const native = await progress(root);
    t.diagnostic(JSON.stringify({ phase: "admission", kind, nativeDeltasAtLeast: native.count, requestedDeltas: count,
      requestedDeltaBytesAtLeast: count * size * 2, isPaused: child.stdout.isPaused(), resumed, deliveredSincePause: rpc.frames.length - beforeFrames, stderr: stderr() }));
    assert.equal(child.stdout.isPaused(), true);
    assert.equal(resumed, false);
    assert.equal(rpc.frames.length, beforeFrames);
    assert.match(stderr(), /output exceeded the allowed size/);
    assert.doesNotMatch(stderr(), /UnhandledPromiseRejection|uncaughtException/);
    assert.deepEqual(await exited, [1, null], stderr());
    const elapsedMs = performance.now() - started;
    assert.ok(elapsedMs < 8000, `overflow close took ${elapsedMs} ms`);
    const outcomes = await Promise.all([first, second].map(thread => storedTurn(root, thread)));
    assert.deepEqual(outcomes.map(turn => turn.status), ["failed", "failed"]);
    assert.equal(rpc.frames.some(f => f.method === "turn/completed" && f.params.turn.status === "completed"), false);
    t.diagnostic(JSON.stringify({ phase: "closed", kind, elapsedMs, exit: child.exitCode, durableStatuses: outcomes.map(turn => turn.status), stderr: stderr() }));
  });
}

test("below-bound stalled stdout still keeps the shipped ten-second write deadline", { timeout: 16_000 }, async t => {
  const { root, child, exited, stderr } = await runtime(t);
  const rpc = conversation(child, stderr);
  const { thread } = await rpc.call("thread/start", { requestId: "deadline", cwd: root, model: "opencode:opencode/free-model" });
  child.stdout.pause();
  const started = performance.now();
  rpc.send("turn/start", { threadId: thread.id, input: [{ type: "text", text: 'burst {"count":32,"size":8192,"hold":true}' }] });
  await until(async () => (await progress(root)).count === 32);
  assert.equal(child.stdout.isPaused(), true);
  assert.deepEqual(await exited, [1, null]);
  const elapsedMs = performance.now() - started;
  assert.ok(elapsedMs >= 9500 && elapsedMs < 14_000, `write deadline took ${elapsedMs} ms`);
  assert.match(stderr(), /Cupola stopped reading OpenCode's response/);
  assert.equal((await storedTurn(root, thread)).status, "failed");
  t.diagnostic(JSON.stringify({ elapsedMs, nativeDeltas: 32, exit: child.exitCode, durableStatus: "failed", stderr: stderr() }));
});

for (const signal of [null, "SIGTERM"]) {
  test(`a paused real stream closes within the shutdown budget on ${signal ?? "EOF"}`, { timeout: 10_000, skip: signal ? NO_SIGTERM : false }, async t => {
    const { root, child, exited, stderr } = await runtime(t);
    const rpc = conversation(child, stderr);
    const { thread } = await rpc.call("thread/start", { requestId: `close-${signal}`, cwd: root, model: "opencode:opencode/free-model" });
    child.stdout.pause();
    rpc.send("turn/start", { threadId: thread.id, input: [{ type: "text", text: 'burst {"count":64,"size":4096,"hold":true}' }] });
    await until(async () => (await progress(root)).count === 64);
    assert.equal(child.stdout.isPaused(), true);
    const started = performance.now();
    if (signal) child.kill(signal); else child.stdin.end();
    assert.deepEqual(await exited, [1, null], stderr());
    const elapsedMs = performance.now() - started;
    assert.ok(elapsedMs < 8000, `shutdown took ${elapsedMs} ms`);
    assert.equal((await storedTurn(root, thread)).status, "failed");
    assert.doesNotMatch(stderr(), /UnhandledPromiseRejection|uncaughtException/);
    t.diagnostic(JSON.stringify({ trigger: signal ?? "EOF", elapsedMs, nativeDeltas: 64, durableStatus: "failed", exit: child.exitCode }));
  });
}

test("the Cursor launch selector serves its own account, native models and turn frames", { timeout: 10_000 }, async t => {
  const { child, exited, stderr } = await runtime(t, { provider: "cursor" });
  const frames = [], pending = new Map();
  const lines = createInterface({ input: child.stdout });
  lines.on("line", line => {
    const frame = JSON.parse(line); frames.push(frame);
    if (frame.id != null && !frame.method) { pending.get(frame.id)?.(frame); pending.delete(frame.id); }
  });
  let id = 0;
  const call = (method, params = {}) => new Promise((resolve, reject) => {
    const next = ++id;
    pending.set(next, frame => frame.error ? reject(new Error(frame.error.message)) : resolve(frame.result));
    child.stdin.write(JSON.stringify({ id: next, method, params }) + "\n");
  });
  assert.equal((await call("initialize")).wonderBridge.family, "cursor");
  assert.equal((await call("account/read")).connected, true);
  const models = (await call("model/list")).data;
  assert.equal(models[0].id, "cursor:default[]");
  assert.ok(models.every(model => model.agentFamily === "cursor"));
  const { thread } = await call("thread/start", { requestId: "cursor-pipes", cwd: process.cwd(), model: models[1].id });
  const { turn } = await call("turn/start", { threadId: thread.id, clientUserMessageId: "cursor-pipes-turn", input: [{ type: "text", text: "hi" }] });
  // Event-driven like the calls above; the test timeout guards a hang.
  if (!frames.some(frame => frame.method === "turn/completed" && frame.params.turnId === turn.id)) {
    await new Promise(resolve => lines.on("line", function done(line) {
      const frame = JSON.parse(line);
      if (frame.method === "turn/completed" && frame.params.turnId === turn.id) { lines.off("line", done); resolve(); }
    }));
  }
  const done = frames.find(frame => frame.method === "turn/completed");
  assert.equal(done.params.agentFamily, "cursor");
  assert.equal(done.params.turn.status, "completed");
  assert.equal(done.params.turn.items.find(item => item.type === "agentMessage").text, "Hello there");
  child.stdin.end();
  assert.deepEqual(await exited, [0, null], stderr());
});


for (const provider of ["opencode", "cursor"]) {
  test(`${provider} bridge passes endpoint and key through to its native CLI`, { timeout: 20_000 }, async t => {
    const requests = [];
    const server = createServer((request, response) => {
      requests.push({ url: request.url, authorization: request.headers.authorization });
      response.end("ok");
    });
    server.listen(0, "127.0.0.1");
    await once(server, "listening");
    t.after(() => { server.closeAllConnections(); server.close(); });
    const { root, child, exited, stderr } = await runtime(t, { provider, endpoint: `http://127.0.0.1:${server.address().port}` });
    const connection = conversation(child, stderr);
    await connection.call("thread/start", { requestId: "endpoint", cwd: root,
      model: provider === "opencode" ? "opencode:local/model0" : "cursor:default[]" });
    assert.ok(requests.some(request => request.url === `/${provider}` && request.authorization === "Bearer fixture-only"));
    child.stdin.end();
    assert.deepEqual(await exited, [0, null], stderr());
  });
}

// Explicit opt-in: node services/acp-runtime/scripts/opencode-live-check.mjs [--model opencode/beam-free]
// Runs the real ACP runtime (main.mjs) against the installed OpenCode: reads the
// account and model list, then one turn with a free model in a scratch folder.
// Refuses a model the catalog does not mark free. No retries, no tools, every
// permission request declined; ~/.wonder and the owner's OpenCode config are untouched.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp, rm, mkdir } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { randomUUID } from "node:crypto";

const flag = process.argv.indexOf("--model");
const wanted = flag > 0 ? process.argv[flag + 1] : "opencode/beam-free";
const root = await mkdtemp(join(tmpdir(), "cupola-opencode-check-"));
const workspace = join(root, "workspace");
await mkdir(workspace);
const child = spawn(process.execPath, [new URL("../main.mjs", import.meta.url).pathname, "--state-dir", join(root, "state"), "--provider", "opencode"], { stdio: ["pipe", "pipe", "inherit"] });
const frames = [], waiting = [];
let buffered = "";
child.stdout.setEncoding("utf8").on("data", text => {
  buffered += text;
  for (let n; (n = buffered.indexOf("\n")) !== -1;) {
    const frame = JSON.parse(buffered.slice(0, n)); buffered = buffered.slice(n + 1);
    frames.push(frame);
    // Decline anything the agent asks the daemon (permissions, tool calls).
    if (frame.method && frame.id != null) child.stdin.write(JSON.stringify({ id: frame.id, result: { decision: "decline" } }) + "\n");
    for (const check of waiting.splice(0)) check();
  }
});
const until = (find, ms = 180_000) => new Promise((resolve, reject) => {
  const timer = setTimeout(() => reject(new Error("timed out; not retrying")), ms);
  const check = () => { const hit = find(); if (hit) { clearTimeout(timer); resolve(hit); } else waiting.push(check); };
  check();
});
const call = async (method, params = {}) => {
  const id = randomUUID();
  child.stdin.write(JSON.stringify({ id, method, params }) + "\n");
  const reply = await until(() => frames.find(frame => frame.id === id && !frame.method));
  if (reply.error) throw new Error(`${method}: ${reply.error.message}`);
  return reply.result;
};
const summary = {};
try {
  await call("initialize");
  const account = await call("account/read", { refresh: true });
  Object.assign(summary, { version: account.runtime.version, availability: account.availability, connected: account.connected, diagnostic: account.diagnostic ?? null });
  assert.equal(account.availability, "ready", JSON.stringify(summary));
  const { data } = await call("model/list");
  summary.models = data.length;
  summary.free = data.filter(m => / · Free$/.test(m.description)).length;
  summary.defaultModel = data.find(m => m.isDefault)?.model ?? null;
  const model = data.find(m => m.model === `opencode:${wanted}`);
  assert(model, `${wanted} is not listed`);
  assert(/ · Free$/.test(model.description), `${wanted} is not marked free; no paid sends`);
  summary.model = model.model;
  const { thread } = await call("thread/start", { requestId: randomUUID(), cwd: workspace, model: model.model });
  const { turn } = await call("turn/start", { threadId: thread.id, clientUserMessageId: randomUUID(), model: model.model,
    input: [{ type: "text", text: "Reply with exactly the word: pong. Do not use any tools." }] });
  const done = await until(() => frames.find(frame => frame.method === "turn/completed" && frame.params.turnId === turn.id));
  const items = done.params.turn.items;
  summary.turn = { status: done.params.turn.status, error: done.params.turn.error ?? null, itemTypes: items.map(item => item.type),
    reply: items.filter(item => item.type === "agentMessage").map(item => item.text).join("\n").slice(0, 200) };
  assert.equal(done.params.turn.status, "completed", JSON.stringify(summary));
  assert(summary.turn.reply.trim(), "empty reply");
  summary.result = "passed";
} catch (error) {
  summary.result = "failed"; summary.error = error.message; process.exitCode = 1;
} finally {
  child.stdin.end();
  await new Promise(resolve => { child.once("exit", resolve); setTimeout(() => child.kill("SIGKILL"), 5000).unref(); });
  await rm(root, { recursive: true, force: true });
  console.log(JSON.stringify(summary, null, 2));
}

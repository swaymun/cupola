// Offline native-history benchmark; no credentials or model calls.
import assert from "node:assert/strict";
import { mkdtemp, mkdir, rm, writeFile } from "node:fs/promises";
import { tmpdir, release, arch } from "node:os";
import { join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { performance } from "node:perf_hooks";

const repo = resolve(process.argv[2] ?? fileURLToPath(new URL("..", import.meta.url)));
const runtime = file => pathToFileURL(join(repo, "services/claude-runtime", file));
const { ClaudeBridge } = await import(runtime("bridge.mjs"));
const { Sessions } = await import(runtime("sessions.mjs"));
const { nativeTurns } = await import(runtime("project-history.mjs"));
const { projectFolderName } = await import(runtime("session-file.mjs"));
const root = await mkdtemp(join(tmpdir(), "wonder-history-profile-"));
let bridge;
try {
  const project = join(root, "project"), native = join(root, "native"), records = join(root, "processes");
  for (const path of [project, native, records]) await mkdir(path);
  const messages = [], id = "11111111-2222-4333-8444-555555555555";
  let prior = null;
  const push = row => { row.parentUuid = prior; prior = row.uuid; messages.push(row); };
  for (let turn = 0; turn < 255; turn++) {
    push({ type: "user", uuid: `u-${turn}`, message: { role: "user", content: `Review the due date handling for task ${turn}.` } });
    for (let tool = 0; tool < 20; tool++) {
      const call = `call-${turn}-${tool}`;
      push({ type: "assistant", uuid: `a-${turn}-${tool}`, message: { id: `m-${turn}-${tool}`, content: [
        { type: "tool_use", id: call, name: "Bash", input: { command: `printf 'Task ${turn} check ${tool}'` } },
      ] } });
      push({ type: "user", uuid: `r-${turn}-${tool}`, message: { role: "user", content: [
        { type: "tool_result", tool_use_id: call, content: "Synthetic test output. ".repeat(55) },
      ] } });
    }
    push({ type: "assistant", uuid: `reply-${turn}`, message: { id: `reply-${turn}`, content: [
      { type: "text", text: "Synthetic findings and follow-up checks. ".repeat(130) },
    ] } });
  }
  const folder = join(native, projectFolderName(project));
  await mkdir(folder, { recursive: true });
  await writeFile(join(folder, `${id}.jsonl`), messages.map(row => JSON.stringify(row)).join("\n") + "\n");
  const sessions = await new Sessions(join(root, "sessions")).initialize();
  const sdk = {
    getSessionInfo: async () => ({ sessionId: id, cwd: project, lastModified: 1 }),
    getSessionMessages: async () => messages,
  };
  bridge = new ClaudeBridge({
    updates: { acquire: async () => ({ runtime: { sdk }, release: async () => {} }) },
    sessions, send: async () => {}, claudeProjectsDir: native, claudeSessionsDir: records,
  });
  const { thread } = await bridge.request("project/session/attach", {
    sessionId: id, model: "claude:haiku", wonderProject: { cwd: project, additionalDirectories: [] },
    wonderPolicy: { mode: "read_only", approvalMode: "ask", readRoots: [project], writeRoots: [], deniedRoots: [] },
  });
  const samples = {};
  const record = (key, start) => (samples[key] ??= []).push(performance.now() - start);
  const walk = async (name, method, params) => {
    let cursor = null, pages = 0, items = 0;
    const start = performance.now();
    do {
      const at = performance.now();
      const result = await bridge.request(method, { threadId: thread.id, ...params, cursor });
      record(name + "Page", at);
      cursor = result.nextCursor;
      items += result.data.length;
      pages++;
      assert.ok(pages <= 100, "history cursor did not finish");
    } while (cursor);
    record(name + "All", start);
    return { pages, items };
  };
  let shape;
  for (let sample = 0; sample < 20; sample++) {
    const start = performance.now();
    nativeTurns(messages);
    record("projection", start);
    const turns = await walk("turns", "thread/turns/list", { itemsView: "notLoaded", limit: 100 });
    const items = await walk("items", "thread/items/list", { limit: 100, sortDirection: "asc" });
    assert.deepEqual(turns, { pages: 3, items: 255 });
    assert.deepEqual(items, { pages: 57, items: 5610 });
    shape = { turns, items };
  }
  const stats = Object.fromEntries(Object.entries(samples).map(([key, values]) => {
    const sorted = [...values].sort((a, b) => a - b);
    return [key, { n: values.length, p50: sorted[Math.ceil(values.length * .5) - 1],
      p95: sorted[Math.ceil(values.length * .95) - 1], max: sorted.at(-1) }];
  }));
  console.log(JSON.stringify({ environment: { node: process.version, os: release(), arch: arch() },
    shape, transcriptMessages: messages.length, stats, samples }, null, 2));
} finally {
  await bridge?.close();
  await rm(root, { recursive: true, force: true });
}

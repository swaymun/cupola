import test from "node:test";
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdtemp, mkdir, writeFile, appendFile, rm, readFile, stat } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { ByteCache, historyFingerprint } from "../cooperative.mjs";
import { ClaudeSessionFiles, projectFolderName } from "../session-file.mjs";
import { ReadWorkers } from "../read-workers.mjs";

test("cursor hashing preserves exact JSON bytes across Unicode boundaries and yields", async () => {
  const shared = { text: "boundary" };
  const data = [{ text: "x".repeat(16383) + "🔎" + "\ud800\n\"\\".repeat(1000000), omitted: undefined,
    array: [undefined, null, Infinity, false, shared], again: shared, date: new Date(0) }];
  const expected = createHash("sha256").update(JSON.stringify(data)).digest("hex").slice(0, 16);
  let ran = false;
  const queued = new Promise(resolve => setImmediate(() => { ran = true; resolve(); }));
  try {
    assert.equal(await historyFingerprint(data), expected);
    assert.equal(ran, true, "queued work progresses while a large fingerprint is being computed");
  } finally { await queued; }
});

test("byte eviction removes oversized entries and releases replacement charges", () => {
  const cache = new ByteCache(100);
  cache.set("old", { text: "old" }, 60); cache.set("new", { text: "new" }, 60);
  assert.deepEqual([...cache.keys()], ["new"]);
  cache.set("new", { text: "smaller" }, 20); cache.set("next", {}, 70);
  assert.equal(cache.bytes, 90);
  cache.set("large", {}, 101);
  assert.deepEqual([...cache.keys()], ["new", "next"], "an oversized read preserves unrelated warm entries");
  assert.equal(cache.bytes, 90);
});

test("an oversized entry with a hold replaces others and expires after idle time", t => {
  t.mock.timers.enable({ apis: ["setTimeout"] });
  const cache = new ByteCache(100, Infinity, { oversizeMs: 1000 });
  cache.set("warm", {}, 60); cache.set("large", {}, 500);
  assert.deepEqual([...cache.keys()], ["large"]);
  t.mock.timers.tick(900);
  cache.set("large", {}, 500); // A later read restarts the hold.
  t.mock.timers.tick(900);
  assert.equal(cache.has("large"), true);
  t.mock.timers.tick(100);
  assert.equal(cache.size, 0);
  assert.equal(cache.bytes, 0);
});

test("large incomplete records stay on disk, finish exactly once and are kept as a compact index", { timeout: 60000 }, async t => {
  const root = await mkdtemp(join(tmpdir(), "claude-large-lines-")), path = join(root, "session.jsonl");
  const files = new ClaudeSessionFiles(root);
  t.after(async () => { await files.close(); await rm(root, { recursive: true, force: true }); });
  files.files.maxBytes = 1024;
  // Over the in-process limit, so the reader process indexes it.
  const row = { type: "assistant", uuid: "large", message: { id: "m", model: "claude-opus-5-5", content: "🔎".repeat(1200000) } };
  const bytes = Buffer.from(JSON.stringify(row) + "\n"), split = bytes.indexOf(Buffer.from("🔎")) + 2;
  await writeFile(path, bytes.subarray(0, split));
  assert.equal((await files.entries(path)).rows.length, 0);
  await appendFile(path, bytes.subarray(split, bytes.length - 1));
  assert.equal((await files.entries(path)).rows.length, 0);
  await appendFile(path, "\n");
  const [first, second] = await Promise.all([files.entries(path), files.entries(path)]);
  assert.equal(first, second);
  assert.deepEqual(first.rows.map(r => [r.uuid, r.at, r.length, r.messageId]), [["large", 0, bytes.length - 1, "m"]]);
  assert.equal(first.settings.turnId, "large");
  assert.ok(first.bytes < 4096, "the index retains no record content");
  assert.equal(files.readers.starts, 1);
  await writeFile(path, '{"uuid":"replacement"}\n');
  assert.deepEqual((await files.entries(path)).rows.map(r => r.uuid), ["replacement"]);
});

// A screenshot-heavy transcript (B-018, A13-02/03, P4-07): every line over
// 256 KiB used to start a worker thread, and a transcript over 8 MiB was
// never cached, so each open, send and history read reparsed all of it.
async function screenshotTranscript(root) {
  const cwd = join(root, "work"), id = "61111111-2222-4333-8444-555555555555";
  const folder = join(root, projectFolderName(cwd)), path = join(folder, `${id}.jsonl`);
  await mkdir(folder, { recursive: true });
  const image = "A".repeat(400 * 1024), lines = [];
  let parent = null;
  for (let i = 0; i < 24; i++) {
    lines.push({ type: "user", uuid: `u${i}`, parentUuid: parent, message: { role: "user", content: `Look ${i}` } });
    lines.push({ type: "assistant", uuid: `a${i}`, parentUuid: `u${i}`, entrypoint: "claude-desktop", effort: "high",
      message: { id: `m${i}`, model: "claude-opus-5-5", role: "assistant", content: [{ type: "tool_use", id: `t${i}`, name: "screenshot", input: {} }] } });
    lines.push({ type: "user", uuid: `r${i}`, parentUuid: `a${i}`, message: { role: "user",
      content: [{ type: "tool_result", tool_use_id: `t${i}`, content: [{ type: "image", source: { type: "base64", data: image } }] }] } });
    parent = `r${i}`;
  }
  await writeFile(path, lines.map(row => JSON.stringify(row)).join("\n") + "\n");
  return { cwd, id, path, parent };
}

function countParses(t) {
  const parse = JSON.parse, counter = { count: 0 };
  JSON.parse = function (...args) { counter.count++; return parse.apply(this, args); };
  t.after(() => { JSON.parse = parse; });
  return counter;
}

test("an 8 MiB+ screenshot transcript is indexed once, in process, and appends parse only new lines", { timeout: 60000 }, async t => {
  const root = await mkdtemp(join(tmpdir(), "claude-screenshots-"));
  const files = new ClaudeSessionFiles(root);
  t.after(async () => { await files.close(); await rm(root, { recursive: true, force: true }); });
  const { cwd, id, path, parent } = await screenshotTranscript(root);
  assert.ok((await stat(path)).size > 8 * 1024 * 1024);
  const parses = countParses(t);
  const index = await files.entries(path);
  assert.equal(index.rows.length, 72);
  assert.equal(files.readers.starts, 0, "no thread or process for records the event loop parses in a few ms");
  assert.ok(files.files.has(path), "the compact index stays cached");
  assert.ok(index.bytes < 64 * 1024, `index charge ${index.bytes}`);
  parses.count = 0;
  assert.equal(await files.entries(path), index);
  assert.equal(parses.count, 0, "an unchanged transcript is not parsed again");
  await appendFile(path, JSON.stringify({ type: "user", uuid: "next", parentUuid: parent, message: { role: "user", content: "More" } }) + "\n");
  const appended = await files.entries(path);
  assert.equal(parses.count, 1, "an append parses only the new line");
  assert.equal(appended.rows.length, 73);
  assert.equal(await files.latestMessageUuid(id, cwd), "next");
  // History before a compaction is read back from disk by range, in order.
  const earlier = await files.earlierMessages(id, cwd, "next");
  assert.equal(earlier.length, 72);
  assert.deepEqual(earlier.slice(0, 3).map(m => m.uuid), ["u0", "a0", "r0"]);
  assert.equal(earlier[2].message.content[0].content[0].source.data.length, 400 * 1024);
});

test("latest settings and message read the transcript tail, not the whole file", { timeout: 60000 }, async t => {
  const root = await mkdtemp(join(tmpdir(), "claude-tail-"));
  const files = new ClaudeSessionFiles(root);
  t.after(async () => { await files.close(); await rm(root, { recursive: true, force: true }); });
  const { cwd, id, path } = await screenshotTranscript(root);
  const parses = countParses(t);
  assert.deepEqual(await files.latestSettings(id, cwd), { turnId: "a23", native: true, model: "claude-opus-5-5", effort: "high", speed: null });
  assert.equal(await files.latestMessageUuid(id, cwd), "r23");
  assert.ok(parses.count <= 3, `parsed ${parses.count} records`);
  assert.equal(files.files.has(path), false, "no full index was needed");
  parses.count = 0;
  await files.latestSettings(id, cwd); await files.latestMessageUuid(id, cwd);
  assert.equal(parses.count, 0, "an unchanged transcript answers from memory");
  // A tail with no reply in reach falls back to the index.
  await appendFile(path, Array.from({ length: 12 }, (_, i) => JSON.stringify({ type: "progress", uuid: `p${i}`, data: "B".repeat(400 * 1024) })).join("\n") + "\n");
  assert.equal((await files.latestSettings(id, cwd)).turnId, "a23");
  assert.equal(files.files.has(path), true);
});

test("one reused reader process serves SDK reads and large records, and retires when idle or closed", { timeout: 60000 }, async t => {
  const root = await mkdtemp(join(tmpdir(), "claude-read-process-")), module = join(root, "sdk.mjs"), marker = join(root, "started");
  const readers = new ReadWorkers({ idleMs: 200 });
  t.after(async () => { await readers.close(); await rm(root, { recursive: true, force: true }); });
  await writeFile(module, `import {appendFile} from 'node:fs/promises';
    export async function getSessionInfo() { return { pid: process.pid }; }
    export async function getSessionMessages(marker) {
      await appendFile(marker, process.pid + '\\n');
      await new Promise(() => {});
    }`);
  await assert.rejects(readers.sdk(module, "query", [marker]), /Unavailable native history reader/);
  await assert.rejects(readFile(marker), { code: "ENOENT" });
  const reused = await readers.usingSdk(module, async sdk => [await sdk.getSessionInfo(), await sdk.getSessionInfo()]);
  const later = await readers.sdk(module, "getSessionInfo", []);
  assert.equal(reused[0].pid, reused[1].pid);
  assert.equal(later.pid, reused[0].pid, "a later lease reuses the same reader");
  const record = join(root, "record.json");
  await writeFile(record, JSON.stringify({ uuid: "r", type: "user" }));
  assert.equal((await readers.json(record)).uuid, "r");
  assert.equal(readers.starts, 1, "large records share the one reader");
  while (readers.reader) await new Promise(resolve => setImmediate(resolve));
  await readers.retiring;
  assert.throws(() => process.kill(later.pid, 0), { code: "ESRCH" }, "an idle reader exits");
  readers.maxUses = 1;
  const a = await readers.sdk(module, "getSessionInfo", []), b = await readers.sdk(module, "getSessionInfo", []);
  assert.notEqual(a.pid, b.pid, "a reader is recycled after its request budget");
  readers.maxUses = 256;
  const first = readers.sdk(module, "getSessionMessages", [marker]);
  const second = readers.sdk(module, "getSessionMessages", [marker]);
  const outcomes = Promise.allSettled([first, second]);
  // The fixture holds the actual worker until the test closes its owner.
  while (!(await readFile(marker, "utf8").catch(() => ""))) await new Promise(resolve => setImmediate(resolve));
  await readers.close();
  assert.deepEqual((await outcomes).map(r => r.status), ["rejected", "rejected"]);
  const pids = (await readFile(marker, "utf8")).trim().split("\n").map(Number);
  assert.equal(pids.length, 1, "the queued native read never starts another process");
  assert.throws(() => process.kill(pids[0], 0), { code: "ESRCH" });
});

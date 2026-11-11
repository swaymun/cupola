import test from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { randomUUID } from "node:crypto";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { RECEIPT_LIMIT, Sessions, headParent } from "../sessions.mjs";

// Storage owns rollback isolation, queued snapshots and retention. Bridge tests
// separately own acknowledgement timing and whether a recovered turn executes.
async function fixture(t) {
  const root = await mkdtemp(join(tmpdir(), "wonder-claude-sessions-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const sessions = await new Sessions(root).initialize();
  const session = await sessions.create({ cwd: root });
  const path = join(root, `${session.id}.json`);
  return { root, sessions, session, path, stored: async () => JSON.parse(await readFile(path, "utf8")) };
}

async function history(f, count = 200) {
  f.session.turns = Array.from({ length: count }, (_, n) => ({ id: randomUUID(), clientUserMessageId: `old-${n}`,
    status: "completed", items: [], error: null, checkpointed: true }));
  f.session.receipts.retired = { id: randomUUID(), status: "completed" };
  f.session.updatedAt = 17;
  await f.sessions.save(f.session);
}

test("lazy discovery defers invalid journals and concurrent loads share recovered receipts", async t => {
  const f = await fixture(t), bad = `claude-${randomUUID()}`;
  await writeFile(join(f.root, `${bad}.json`), JSON.stringify({ schemaVersion: 999 }));
  const cold = await new Sessions(f.root).initialize();
  assert.equal(cold.values.size, 0);
  const [one, two] = await Promise.all([cold.load(f.session.id), cold.load(f.session.id)]);
  assert.equal(one, two);
  assert.equal(cold.values.size, 1);
  await assert.rejects(cold.load(bad), /incompatible/);
  assert.equal(await cold.load(f.session.id), one);
});

// The sidebar reads a few native sessions among hundreds of journals; only the
// matching journals load, and a native ID changed since the last save is found.
test("a native session read loads only the journals that may match", async t => {
  const f = await fixture(t);
  for (let n = 0; n < 5; n++) await f.sessions.create({ cwd: f.root });
  const unordered = `claude-${randomUUID()}`;
  await writeFile(join(f.root, `${unordered}.json`), JSON.stringify({ id: unordered, schemaVersion: 1, sdkSessionId: randomUUID(),
    createdAt: 1, options: { cwd: f.root }, parent: null, turns: [], receipts: {} }));
  const cold = await new Sessions(f.root).initialize();
  const loaded = [], read = cold.readSession.bind(cold);
  cold.readSession = id => { loaded.push(id); return read(id); };
  const found = [];
  for await (const session of cold.withNative([f.session.sdkSessionId])) if (session.sdkSessionId === f.session.sdkSessionId) found.push(session.id);
  assert.deepEqual(found, [f.session.id]);
  assert.equal(loaded.length, 2, "the match and the journal whose head names nothing");
  const moved = await cold.load(f.session.id);
  moved.sdkSessionId = randomUUID();
  assert.equal(await cold.findId(s => s.sdkSessionId === moved.sdkSessionId, [moved.sdkSessionId]), f.session.id,
    "a loaded session is a candidate before its next save");
  await cold.save(moved);
  cold.values.clear();
  assert.equal(await cold.findId(s => s.sdkSessionId === moved.sdkSessionId, [moved.sdkSessionId]), f.session.id);
  assert.equal(await cold.findId(s => s.sdkSessionId === f.session.sdkSessionId, [f.session.sdkSessionId]), null);
});

test("byte eviction pins active turns and writes, then reloads durable duplicate receipts", async t => {
  const f = await fixture(t);
  f.sessions.maxBytes = 1;
  const { turn } = await f.sessions.accept(f.session, { clientUserMessageId: "owned" });
  await f.sessions.create({ cwd: f.root }); f.sessions.evict();
  assert.equal(await f.sessions.load(f.session.id), f.session, "the active run keeps its mutable identity");
  turn.status = "completed";
  const gate = Promise.withResolvers(); f.sessions.writes.set(f.session.id, gate.promise);
  const writing = f.sessions.save(f.session); f.sessions.evict();
  assert.equal(f.sessions.get(f.session.id), f.session, "an unfinished writer cannot be evicted");
  gate.resolve(); await writing; f.sessions.evict();
  assert.equal(f.sessions.values.has(f.session.id), false);
  await f.sessions.using(f.session.id, async session => {
    assert.notEqual(session, f.session);
    assert.equal(await f.sessions.load(session.id), session);
    const replay = await f.sessions.accept(session, { clientUserMessageId: "owned" });
    assert.equal(replay.duplicate, true); assert.equal(replay.turn.id, turn.id);
  });
  assert.equal(f.sessions.values.size, 0);
  assert.equal(f.sessions.cacheBytes, 0);
});

test("an evicted session object cannot overwrite a newer journal or keep a byte charge", async t => {
  const f = await fixture(t);
  f.sessions.maxBytes = 1;
  const stale = f.session;
  f.sessions.evict();
  assert.equal(f.sessions.values.has(stale.id), false);
  // A newer object accepts a turn, finishes it and is evicted in turn.
  await f.sessions.using(stale.id, async session => {
    const { turn } = await f.sessions.accept(session, { clientUserMessageId: "msg-1" });
    turn.status = "completed";
    await f.sessions.save(session);
  });
  assert.equal(f.sessions.values.has(stale.id), false);
  await assert.rejects(f.sessions.save(stale), /ownership changed/);
  assert.deepEqual(Object.keys((await f.stored()).receipts), ["msg-1"]);
  assert.equal(f.sessions.charges.size, 0);
  assert.equal(f.sessions.cacheBytes, 0);
  const retry = await f.sessions.using(stale.id, session => f.sessions.accept(session, { clientUserMessageId: "msg-1" }));
  assert.equal(retry.duplicate, true);
});

for (const following of ["save", "steering accept"]) {
  test(`failed admission preserves history and unrelated updates in a queued ${following}`, async t => {
    const f = await fixture(t);
    await history(f);
    const oldest = f.session.turns[0], earlier = f.session.turns.at(-1);
    const retired = structuredClone(f.session.receipts.retired);
    earlier.status = "inProgress";
    const gate = Promise.withResolvers(), recovered = Promise.withResolvers();
    const blocker = `${f.path}.${process.pid}.tmp`;
    await mkdir(blocker);
    f.sessions.writes.set(f.session.id, gate.promise);
    const failed = f.sessions.accept(f.session, { clientUserMessageId: "failed" }, { steering: true, checkpointed: true });
    const duplicate = f.sessions.accept(f.session, { clientUserMessageId: "failed" }, { steering: true });
    const rejected = Promise.allSettled([failed, duplicate]);
    // Capture a later snapshot now, but hold its writer until storage recovers.
    const failedWrite = f.sessions.writes.get(f.session.id);
    f.sessions.writes.set(f.session.id, failedWrite.catch(() => {}).then(() => recovered.promise));
    earlier.status = "completed";
    earlier.items.push({ id: "earlier-result", type: "agentMessage", text: "Earlier work finished" });
    f.session.sdkStarted = true;
    f.session.updatedAt = 91;
    const later = following === "save" ? f.sessions.save(f.session)
      : f.sessions.accept(f.session, { clientUserMessageId: "later" }, { steering: true, checkpointed: true });
    try {
      gate.resolve();
      const outcomes = await rejected;
      for (const outcome of outcomes) {
        assert.equal(outcome.status, "rejected");
        assert.equal(outcome.reason.code, "EISDIR");
      }
      assert.equal(outcomes[0].reason, outcomes[1].reason);
      assert.equal(f.session.turns.some(turn => turn.clientUserMessageId === "failed"), false);
      assert.equal(f.session.receipts.failed, undefined);
      assert.equal(f.session.turns.includes(earlier), true, "rollback preserves references used by the running bridge");
      assert.equal(earlier.items[0].text, "Earlier work finished");
      assert.equal(f.session.sdkStarted, true);
      assert.equal(f.session.updatedAt, 91, "rollback cannot undo another writer's update");
      await rm(blocker, { recursive: true });
      recovered.resolve();
      const accepted = await later;
      const stored = await f.stored();
      assert.equal(stored.turns.length, 200);
      assert.equal(f.session.turns.length, 200);
      assert.equal(stored.turns.some(turn => turn.clientUserMessageId === "failed"), false);
      assert.equal(stored.receipts.failed, undefined, "a queued snapshot cannot resurrect a failed receipt");
      assert.deepEqual(stored.receipts.retired, retired);
      assert.deepEqual(stored.turns.find(turn => turn.id === earlier.id), earlier);
      assert.equal(stored.sdkStarted, true);
      if (following === "save") {
        assert.equal(stored.turns[0].id, oldest.id, "a failed capped admission must not discard the oldest valid turn");
        assert.equal(stored.updatedAt, 91);
      } else {
        assert.equal(stored.turns[0].clientUserMessageId, "old-1");
        assert.equal(stored.turns.at(-1).id, accepted.turn.id);
        assert.equal(stored.turns.at(-1).checkpointed, true);
      }
      const restarted = await new Sessions(f.root).initialize();
      assert.equal((await restarted.load(f.session.id)).turns.length, 200);
      assert.equal((await restarted.load(f.session.id)).receipts.failed, undefined);
      assert.equal((await restarted.accept((await restarted.load(f.session.id)), { clientUserMessageId: oldest.clientUserMessageId })).duplicate, true);
    } finally {
      gate.resolve(); recovered.resolve();
      await Promise.allSettled([failed, duplicate, later]);
    }
  });
}

test("successful steering retains checkpoints, 200 turns and older replay receipts across restart", async t => {
  const f = await fixture(t);
  await history(f);
  const oldest = f.session.turns[0], running = f.session.turns.at(-1);
  running.status = "inProgress";
  const { turn } = await f.sessions.accept(f.session, { clientUserMessageId: "guide" }, { steering: true, checkpointed: true });
  assert.equal(f.session.turns.length, 200);
  assert.equal(f.session.turns.includes(running), true);
  assert.equal(turn.checkpointed, true);
  assert.equal((await f.stored()).turns.at(-1).checkpointed, true);
  const restarted = await new Sessions(f.root).initialize();
  const session = (await restarted.load(f.session.id));
  assert.equal(session.turns.at(-1).status, "interrupted");
  assert.equal(session.turns.at(-1).checkpointed, true);
  const replay = await restarted.accept(session, { clientUserMessageId: oldest.clientUserMessageId });
  assert.equal(replay.duplicate, true);
  assert.equal(replay.turn.id, oldest.id);
  assert.equal((await restarted.accept(session, { clientUserMessageId: "guide" })).duplicate, true);
  assert.equal(session.turns.length, 200);
});

test("a delayed journal in one session does not block accepting another session", { timeout: 5000 }, async t => {
  const f = await fixture(t);
  const other = await f.sessions.create({ cwd: f.root });
  const gate = Promise.withResolvers();
  f.sessions.writes.set(f.session.id, gate.promise);
  const delayed = f.sessions.accept(f.session, { clientUserMessageId: "delayed" });
  try {
    const accepted = await f.sessions.accept(other, { clientUserMessageId: "independent" });
    const stored = JSON.parse(await readFile(join(f.root, `${other.id}.json`), "utf8"));
    assert.equal(stored.turns[0].id, accepted.turn.id);
    assert.deepEqual((await f.stored()).turns, []);
  } finally {
    gate.resolve();
    await delayed;
  }
});

// Every save rewrites the whole journal, so receipts cannot outgrow the turn
// window forever. Live-turn receipts and a recent tail of evicted ones stay.
test("saves keep receipts bounded while retries inside the window still deduplicate", async t => {
  const f = await fixture(t);
  await history(f);
  for (let n = 0; n < 5000; n++) f.session.receipts[`legacy-${n}`] = { id: randomUUID(), status: "completed" };
  await f.sessions.save(f.session);
  const stored = await f.stored();
  const receiptBytes = JSON.stringify(stored.receipts).length;
  assert.equal(Object.keys(stored.receipts).length, RECEIPT_LIMIT);
  for (const turn of f.session.turns) assert.equal(stored.receipts[turn.clientUserMessageId].id, turn.id, "live turns keep their receipts");
  assert.ok(stored.receipts["legacy-4999"] && !stored.receipts["legacy-0"] && !stored.receipts.retired);

  // New messages push the oldest evicted receipts out rather than growing the file.
  for (let n = 0; n < 5; n++) {
    const { turn } = await f.sessions.accept(f.session, { clientUserMessageId: `new-${n}` });
    turn.status = "completed";
  }
  await f.sessions.save(f.session);
  const grown = (await f.stored()).receipts;
  assert.equal(Object.keys(grown).length, RECEIPT_LIMIT);
  assert.ok(JSON.stringify(grown).length < receiptBytes * 1.05, "receipt bytes stay flat as messages are added");

  const recent = await f.sessions.accept(f.session, { clientUserMessageId: "legacy-4999" });
  assert.equal(recent.duplicate, true, "a retry inside the window is recognized");
  const evicted = await f.sessions.accept(f.session, { clientUserMessageId: "legacy-0" });
  assert.equal(evicted.duplicate, false, "a receipt outside the window no longer deduplicates");
});

test("loading an oversized journal trims receipts without touching live turns", async t => {
  const f = await fixture(t);
  await history(f, 3);
  const journal = await f.stored();
  for (let n = 0; n < 3000; n++) journal.receipts[`legacy-${n}`] = { id: randomUUID(), status: "completed" };
  await writeFile(f.path, JSON.stringify(journal));
  const session = (await (await new Sessions(f.root).initialize()).load(f.session.id));
  assert.equal(Object.keys(session.receipts).length, RECEIPT_LIMIT);
  for (const turn of session.turns) assert.equal(session.receipts[turn.clientUserMessageId].id, turn.id);
});

test("a journal head names its parent only from the top-level key", () => {
  const parent = { threadId: "claude-parent", depth: 1, name: "Scout \\ \"one\"" };
  const head = (options, value = parent) => JSON.stringify({ schemaVersion: 1, id: "claude-child", sdkSessionId: "native",
    sdkStarted: false, createdAt: 1, options, parent: value, turns: [{ items: [{ text: "x".repeat(100) }] }], receipts: {} });
  const decoy = { instructions: '"parent":{"threadId":"claude-decoy"}', nested: { parent: { threadId: "claude-decoy" } }, list: [{ parent: null }] };
  assert.deepEqual(headParent(head(decoy)), parent);
  assert.equal(headParent(head(decoy, null)), null);
  const full = head(decoy);
  assert.equal(headParent(full.slice(0, full.indexOf('"parent":{"threadId":"claude-parent"') + 20)), undefined, "a cut head is not settled");
  assert.equal(headParent(JSON.stringify({ schemaVersion: 1, id: "claude-child", turns: [], parent })), undefined, "turns first is not settled");
  assert.equal(headParent(JSON.stringify({ schemaVersion: 1, parent: { depth: 1 } })), undefined);
});

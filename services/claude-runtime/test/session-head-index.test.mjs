import assert from "node:assert/strict";
import { appendFile, mkdtemp, rm, utimes } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { ClaudeSessionFiles } from "../session-file.mjs";
import { fixtureId, writeSessionFixture, writeTranscript } from "./session-fixture.mjs";

// The real SDK reads synthetic transcripts under this test's own config.
// Node runs this file's tests serially, so the environment variable is safe.
async function indexed(t, { files = 40, every = 1, ...fixture } = {}) {
  const root = await mkdtemp(join(tmpdir(), "session-head-index-"));
  const previous = process.env.CLAUDE_CONFIG_DIR;
  process.env.CLAUDE_CONFIG_DIR = root;
  t.after(async () => {
    if (previous === undefined) delete process.env.CLAUDE_CONFIG_DIR; else process.env.CLAUDE_CONFIG_DIR = previous;
    await rm(root, { recursive: true, force: true });
  });
  const cwd = "/work/Repo", sdk = await import("@anthropic-ai/claude-agent-sdk");
  const made = await writeSessionFixture(root, cwd, { files, every, bytes: 64 * 1024, ...fixture });
  const calls = { list: [], entrypoint: 0 };
  const list = (...args) => (calls.list.push(args[0]), sdk.listSessions(...args));
  const sessions = new ClaudeSessionFiles(join(root, "projects"), action => action({ ...sdk, listSessions: list }));
  const probe = sessions.producerEntrypoint.bind(sessions);
  sessions.producerEntrypoint = (...args) => (calls.entrypoint++, probe(...args));
  // A new instance has no index, which is what a restarted runtime sees.
  const fresh = () => new ClaudeSessionFiles(join(root, "projects"), action => action({ ...sdk, listSessions: list }));
  return { root, cwd, sessions, calls, fresh, ...made };
}

const ids = page => page.data.map(row => row.sessionId);
// A completed first page is served while the folder is read again in the
// background when no file was added or removed (an append does not touch the
// folder), so a change to a file shows on the request after that read.
async function afterRefresh(sessions, read) {
  await read();
  await Promise.all([...sessions.catalogReads.values()].map(pending => pending.promise?.catch(() => {})));
  return read();
}
async function pages(sessions, cwd, limit) {
  const all = []; let cursor = null;
  do {
    const page = await sessions.listSessions({ dirs: [cwd], limit, cursor });
    assert.equal(page.partial, false);
    all.push(...ids(page)); cursor = page.nextCursor;
  } while (cursor);
  return all;
}

test("every page size and cursor path lists the same chats in the same order", async t => {
  const { sessions, cwd, chats } = await indexed(t, { files: 40, every: 1 });
  assert.equal(chats.length, 40);
  const all = await sessions.listSessions({ dirs: [cwd], limit: 100 });
  assert.deepEqual(ids(all), chats);
  for (const limit of [1, 5, 17, 30, 40, 41]) assert.deepEqual(await pages(sessions, cwd, limit), chats, `limit ${limit}`);
  // The first 17 rows are the same whether the page stops there or goes on.
  assert.deepEqual(ids(await sessions.listSessions({ dirs: [cwd], limit: 17 })), chats.slice(0, 17));
  assert.deepEqual(ids(await sessions.listSessions({ dirs: [cwd], limit: 30 })), chats.slice(0, 30));
});

test("asking past the last chat reads neither the folder's transcripts nor the SDK again", async t => {
  // 300 transcripts, 17 of them chats: the rest are one-shot runs.
  const { sessions, cwd, calls, chats } = await indexed(t, { files: 300, every: 18 });
  assert.equal(chats.length, 17);
  const first = await sessions.listSessions({ dirs: [cwd], limit: 17 });
  assert.deepEqual(ids(first), chats);
  assert.equal(calls.list.length, 1, "the folder is listed once, when first seen");
  assert.equal(calls.list[0].limit, 300);
  // Proving the end probes the other 283 transcripts, once.
  assert.deepEqual(await pages(sessions, cwd, 30), chats);
  assert.equal(calls.entrypoint, 300);
  const probes = calls.entrypoint;
  // Later requests (a refresh, the page past the last row, any page size)
  // stat the files and find nothing new to read.
  for (const limit of [17, 30, 5]) assert.deepEqual(await pages(sessions, cwd, limit), chats);
  const past = await sessions.listSessions({ dirs: [cwd], limit: 17, cursor: first.nextCursor });
  assert.deepEqual([past.data, past.nextCursor, past.partial], [[], null, false]);
  assert.equal(calls.list.length, 1, "no SDK listing for an unchanged folder");
  assert.equal(calls.entrypoint, probes, "no transcript is probed twice");
});

test("a transcript that is added, grows or is deleted shows up on the next request, and only it is read", async t => {
  const { sessions, cwd, calls, chats, folder, fresh } = await indexed(t, { files: 60, every: 6 });
  const newest = 1_700_000_000;
  assert.deepEqual(await pages(sessions, cwd, 100), chats);
  const listed = calls.list.length, probed = calls.entrypoint;

  // Added: the newest transcript. Only the head of the folder is listed.
  const added = fixtureId(1000);
  await writeTranscript(folder, cwd, added, { at: newest + 100, bytes: 64 * 1024 });
  assert.deepEqual(await pages(sessions, cwd, 100), [added, ...chats]);
  assert.equal(calls.list.length, listed + 1);
  assert.equal(calls.list.at(-1).limit, 1, "one new file reads one row, not the folder");
  assert.equal(calls.entrypoint, probed + 1, "and probes one transcript");

  // Grown: an older chat gets a new reply, so it becomes the newest.
  const grown = chats[3], path = join(folder, `${grown}.jsonl`);
  await appendFile(path, JSON.stringify({ type: "assistant", sessionId: grown, uuid: "later", parentUuid: "u", message: { role: "assistant", content: "More" } }) + "\n");
  await utimes(path, newest + 200, newest + 200);
  const reordered = [grown, added, ...chats.filter(id => id !== grown)];
  assert.deepEqual(await afterRefresh(sessions, () => pages(sessions, cwd, 100)), reordered);
  assert.equal((await sessions.listSessions({ dirs: [cwd], limit: 1 })).data[0].updatedAt, newest + 200);
  assert.equal(calls.list.at(-1).limit, 1, "one changed file reads one row, not the folder");

  // Grown with its old mtime restored: the size alone shows it changed.
  const quiet = chats[5], quietPath = join(folder, `${quiet}.jsonl`), quietAt = newest - 5 * 6 * 60;
  const before = calls.list.length;
  await appendFile(quietPath, "\n");
  await utimes(quietPath, quietAt, quietAt);
  await afterRefresh(sessions, () => pages(sessions, cwd, 100));
  assert.equal(calls.list.length, before + 1);
  assert.deepEqual(await pages(sessions, cwd, 100), reordered);

  // Deleted: gone from the list, with no SDK read at all.
  await rm(join(folder, `${added}.jsonl`));
  const remaining = reordered.filter(id => id !== added);
  const afterDelete = calls.list.length;
  assert.deepEqual(await pages(sessions, cwd, 100), remaining);
  assert.equal(calls.list.length, afterDelete);

  // A fresh instance (a restarted runtime) agrees.
  assert.deepEqual(await pages(fresh(), cwd, 100), remaining);
});

test("a one-shot run is never listed, and a transcript that becomes a chat is", async t => {
  const { sessions, cwd, folder, chats } = await indexed(t, { files: 30, every: 10 });
  assert.deepEqual(await pages(sessions, cwd, 100), chats);
  const run = fixtureId(2000);
  await writeTranscript(folder, cwd, run, { entrypoint: "sdk-cli", at: 1_700_000_500, bytes: 64 * 1024 });
  assert.deepEqual(await pages(sessions, cwd, 100), chats);
  // The same file rewritten by an interactive session is a chat.
  await writeTranscript(folder, cwd, run, { entrypoint: "cli", at: 1_700_000_600, bytes: 64 * 1024 });
  assert.deepEqual(await afterRefresh(sessions, () => pages(sessions, cwd, 100)), [run, ...chats]);
});

test("folders the SDK names differently, or that are missing, are listed by the SDK as before", async t => {
  const { sessions, calls } = await indexed(t, { files: 3, every: 1 });
  const page = await sessions.listSessions({ dirs: ["/work/NoSuchFolder"], limit: 10 });
  assert.deepEqual([page.data, page.nextCursor, page.partial], [[], null, false]);
  assert.deepEqual(calls.list.at(-1), { dir: "/work/NoSuchFolder", includeWorktrees: false, includeProgrammatic: true, offset: 0, limit: 100 });
});

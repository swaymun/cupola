import { spawn, execFileSync } from "node:child_process";
import test from "node:test";
import assert from "node:assert/strict";
import { appendFile, mkdtemp, mkdir, readFile, rm, stat, utimes, writeFile, symlink, realpath } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { createRequire } from "node:module";
import { Sessions } from "../sessions.mjs";
import { ClaudeBridge, promptContext, selectedModel, withLiveRoster } from "../bridge.mjs";
import { NESTED_SANDBOX_GUIDANCE, ToolPolicy } from "../permissions.mjs";
import { backgroundTasks, claudeSessionBusy, claudeSessionState, mainTurnRunning, nativeTurns, sessionSummary, turnEndMessageId } from "../project-history.mjs";
import { ClaudeSessionFiles, projectFolderName } from "../session-file.mjs";
import { TurnProjection } from "../projection.mjs";
import { BUSY_MESSAGE, LineFramer, serveFrames } from "../frames.mjs";

// The `procStart` Claude Code records: `ps` lstart, or the creation FILETIME on Windows.
const processLabels = new Map();
const processLabel = pid => processLabels.get(pid) ?? processLabels.set(pid, process.platform === "win32"
  ? execFileSync("powershell.exe", ["-NoProfile", "-Command", `[Diagnostics.Process]::GetProcessById(${pid}).StartTime.ToFileTimeUtc()`], { encoding: "utf8" }).trim()
  : execFileSync("/bin/ps", ["-o", "lstart=", "-p", String(pid)], { encoding: "utf8", env: { ...process.env, LC_ALL: "C", TZ: "UTC" } }).trim()).get(pid);

// The native SDK readers run only against this test's synthetic config. No
// query/model work is started. Node runs this file's tests serially.
async function nativeFiles(t, root) {
  const previous = process.env.CLAUDE_CONFIG_DIR;
  process.env.CLAUDE_CONFIG_DIR = root;
  t.after(() => { if (previous === undefined) delete process.env.CLAUDE_CONFIG_DIR; else process.env.CLAUDE_CONFIG_DIR = previous; });
  const sdk = await import("@anthropic-ai/claude-agent-sdk");
  return new ClaudeSessionFiles(join(root, "projects"), action => action(sdk));
}

test("the production SDK facade reads the exact native transcript in its leased reader", async t => {
  const root = await mkdtemp(join(tmpdir(), "claude-leased-read-"));
  const cwd = join(root, "work"), id = "51111111-2222-4333-8444-555555555555";
  const folder = join(root, "projects", projectFolderName(cwd));
  await mkdir(folder, { recursive: true });
  const rows = [
    { type: "user", uuid: "u1", parentUuid: null, sessionId: id, cwd, message: { role: "user", content: "Read only" } },
    { type: "assistant", uuid: "a1", parentUuid: "u1", sessionId: id, cwd, message: { role: "assistant", content: [{ type: "text", text: "Exact native answer" }] } },
  ];
  await writeFile(join(folder, `${id}.jsonl`), rows.map(row => JSON.stringify(row)).join("\n") + "\n");
  await nativeFiles(t, root);
  const sdk = await import("@anthropic-ai/claude-agent-sdk"), entry = createRequire(import.meta.url).resolve("@anthropic-ai/claude-agent-sdk");
  const sessions = await new Sessions(join(root, "sessions")).initialize();
  let released = 0;
  const bridge = new ClaudeBridge({ sessions, send: async () => {}, updates: {
    acquire: async () => ({ runtime: { sdk, entry }, release: async () => { released++; } }),
  } });
  t.after(async () => { await bridge.close(); await rm(root, { recursive: true, force: true }); });
  const result = await bridge.withSdk(native => native.getSessionMessages(id, { dir: cwd }));
  assert.deepEqual(result.map(row => row.uuid), ["u1", "a1"]);
  assert.equal(result[1].message.content[0].text, "Exact native answer");
  assert.equal(released, 1);
});

// A reader process costs about 0.1 s to start. A transcript small enough to
// parse without a long event-loop pause, and a roster with no agent files,
// start none; a larger transcript still reads in the leased process.
test("a small transcript and an empty agent roster read without a reader process", async t => {
  const root = await realpath(await mkdtemp(join(tmpdir(), "claude-inline-read-")));
  const cwd = join(root, "work"), id = "51111111-2222-4333-8444-555555555556";
  const folder = join(root, "projects", projectFolderName(cwd));
  await mkdir(folder, { recursive: true }); await mkdir(cwd);
  const common = { sessionId: id, cwd, isSidechain: false, timestamp: "2026-10-13T10:00:00Z" };
  await writeFile(join(folder, `${id}.jsonl`), [
    { type: "user", uuid: "u1", parentUuid: null, message: { role: "user", content: "Read only" } },
    { type: "assistant", uuid: "a1", parentUuid: "u1", message: { role: "assistant", id: "m1", stop_reason: "end_turn", content: [{ type: "text", text: "Inline answer" }] } },
  ].map(row => JSON.stringify({ ...common, ...row })).join("\n") + "\n");
  await nativeFiles(t, root);
  const sdk = await import("@anthropic-ai/claude-agent-sdk"), entry = createRequire(import.meta.url).resolve("@anthropic-ai/claude-agent-sdk");
  const sessions = await new Sessions(join(root, "sessions")).initialize();
  const bridge = new ClaudeBridge({ sessions, send: async () => {}, claudeProjectsDir: join(root, "projects"), claudeSessionsDir: join(root, "records"),
    updates: { acquire: async () => ({ runtime: { sdk, entry }, release: async () => {} }) } });
  t.after(async () => { await bridge.close(); await rm(root, { recursive: true, force: true }); });
  const spawned = [], leased = bridge.readers.usingSdk.bind(bridge.readers);
  bridge.readers.usingSdk = (owner, action) => leased(owner, readers => action(Object.fromEntries(Object.entries(readers)
    .map(([name, read]) => [name, (...args) => { spawned.push(name); return read(...args); }]))));
  const { thread } = await bridge.request("project/session/attach", { sessionId: id, model: "claude:haiku",
    wonderProject: { cwd, additionalDirectories: [] }, wonderPolicy: { mode: "read_only", approvalMode: "ask", readRoots: [cwd], writeRoots: [], deniedRoots: [] } });
  const history = () => bridge.request("thread/turns/list", { threadId: thread.id, includeHistoryRevision: true });
  assert.equal((await history()).data[0].items.at(-1).text, "Inline answer");
  assert.deepEqual(spawned, [], "no reader process for a small transcript or an empty roster");
  bridge.inProcessReadBytes = 0; bridge.transcripts.clear();
  assert.equal((await history()).data[0].items.at(-1).text, "Inline answer");
  assert.deepEqual(spawned, ["getSessionMessages"], "a larger transcript still reads in the leased process");
});

// Claude on the Mac keeps writing while a chat is open (A13-03). A history
// read that sees the transcript grow reads it again instead of failing with
// "Claude is updating"; a screenshot-heavy transcript reads fast enough to.
test("history read while Claude appends to a large transcript returns the newer history", { timeout: 60000 }, async t => {
  const root = await realpath(await mkdtemp(join(tmpdir(), "claude-append-read-")));
  const cwd = join(root, "work"), id = "51111111-2222-4333-8444-555555555557";
  const folder = join(root, "projects", projectFolderName(cwd)), path = join(folder, `${id}.jsonl`);
  await mkdir(folder, { recursive: true }); await mkdir(cwd);
  const common = { sessionId: id, cwd, isSidechain: false, timestamp: "2026-10-13T10:00:00Z" };
  const line = row => JSON.stringify({ ...common, ...row }) + "\n", image = "A".repeat(400 * 1024);
  let text = "", parent = null;
  for (let i = 0; i < 22; i++) {
    text += line({ type: "user", uuid: `u${i}`, parentUuid: parent, message: { role: "user", content: `Look ${i}` } });
    text += line({ type: "assistant", uuid: `a${i}`, parentUuid: `u${i}`, message: { role: "assistant", id: `m${i}`, model: "claude-opus-5-5", stop_reason: "tool_use",
      content: [{ type: "tool_use", id: `t${i}`, name: "Read", input: { file_path: "shot.png" } }] } });
    text += line({ type: "user", uuid: `r${i}`, parentUuid: `a${i}`, message: { role: "user",
      content: [{ type: "tool_result", tool_use_id: `t${i}`, content: [{ type: "image", source: { type: "base64", media_type: "image/png", data: image } }] }] } });
    text += line({ type: "assistant", uuid: `e${i}`, parentUuid: `r${i}`, message: { role: "assistant", id: `n${i}`, model: "claude-opus-5-5", stop_reason: "end_turn",
      content: [{ type: "text", text: `Seen ${i}` }] } });
    parent = `e${i}`;
  }
  await writeFile(path, text);
  assert.ok((await stat(path)).size > 8 * 1024 * 1024);
  await nativeFiles(t, root);
  const sdk = await import("@anthropic-ai/claude-agent-sdk"), entry = createRequire(import.meta.url).resolve("@anthropic-ai/claude-agent-sdk");
  const sessions = await new Sessions(join(root, "sessions")).initialize();
  const bridge = new ClaudeBridge({ sessions, send: async () => {}, claudeProjectsDir: join(root, "projects"), claudeSessionsDir: join(root, "records"),
    updates: { acquire: async () => ({ runtime: { sdk, entry }, release: async () => {} }) } });
  t.after(async () => { await bridge.close(); await rm(root, { recursive: true, force: true }); });
  const { thread } = await bridge.request("project/session/attach", { sessionId: id, model: "claude:haiku",
    wonderProject: { cwd, additionalDirectories: [] }, wonderPolicy: { mode: "read_only", approvalMode: "ask", readRoots: [cwd], writeRoots: [], deniedRoots: [] } });
  // Claude writes the next exchange while the first read is in progress.
  const context = bridge.sessionFiles.messageContext.bind(bridge.sessionFiles);
  let appends = 0;
  bridge.sessionFiles.messageContext = async (...args) => {
    if (!appends++) await appendFile(path, line({ type: "user", uuid: "u-new", parentUuid: parent, message: { role: "user", content: "One more" } })
      + line({ type: "assistant", uuid: "a-new", parentUuid: "u-new", message: { role: "assistant", id: "m-new", model: "claude-opus-5-5", stop_reason: "end_turn",
        content: [{ type: "text", text: "Written while reading" }] } }));
    return context(...args);
  };
  const started = performance.now();
  const turns = (await bridge.request("thread/turns/list", { threadId: thread.id, includeHistoryRevision: true })).data;
  const elapsed = performance.now() - started;
  assert.equal(appends, 2, "the grown transcript was read again");
  assert.equal(turns.at(0).items.at(-1).text, "Written while reading");
  assert.ok(turns.some(turn => turn.items.some(item => item.text === "Seen 0")));
  // Unchanged: answered from the cached read.
  const again = (await bridge.request("thread/turns/list", { threadId: thread.id, includeHistoryRevision: true })).data;
  assert.equal(appends, 2);
  assert.equal(again.at(0).items.at(-1).text, "Written while reading");
  t.diagnostic(`two reads of a ${Math.round((await stat(path)).size / 1048576)} MiB transcript: ${Math.round(elapsed)} ms`);
});

// Contract: badges read compact native task metadata, never SDK history,
// even with a large tool result, and still revalidate output and liveness.
test("task badges read incremental metadata without materializing SDK history", async t => {
  const id = "11111111-2222-4333-8444-555555555559";
  const transcripts = {};
  const f = await fixture(t, { transcripts, nativeTaskFiles: true });
  transcripts[id] = { cwd: f.primary, messages: [] };
  const { thread } = await f.bridge.request("project/session/attach", { sessionId: id, model: "claude:haiku", wonderPolicy: f.policy, wonderProject: f.project });
  const folder = join(f.root, "claude-projects", projectFolderName(f.primary));
  const path = join(folder, `${id}.jsonl`);
  await mkdir(folder, { recursive: true });
  const line = row => JSON.stringify({ sessionId: id, cwd: f.primary, ...row }) + "\n";
  const rows = [
    { type: "user", uuid: "u1", parentUuid: null, message: { role: "user", content: [{ type: "image", source: { type: "base64", data: "AQID", media_type: "image/png" } }] } },
    { type: "assistant", uuid: "a1", parentUuid: "u1", message: { id: "m1", role: "assistant", content: [
      { type: "tool_use", id: "bg", name: "Bash", input: { command: "make", run_in_background: true } },
      { type: "tool_use", id: "read", name: "Read", input: { file_path: "large" } }] } },
    { type: "user", uuid: "r1", parentUuid: "a1", message: { role: "user", content: [
      { type: "tool_result", tool_use_id: "bg", content: "Command running in background with ID: build" },
      { type: "tool_result", tool_use_id: "read", content: "x".repeat(9 * 1024 * 1024) }] } },
    { type: "assistant", uuid: "a2", parentUuid: "r1", message: { id: "m2", role: "assistant", model: "claude-sonnet-4-6", stop_reason: "end_turn", content: [{ type: "text", text: "Started" }] } },
  ];
  await writeFile(path, rows.map(line).join(""));
  const records = join(f.root, "native-sessions");
  await mkdir(records, { recursive: true });
  await writeFile(join(records, `${process.pid}.json`), JSON.stringify({ pid: process.pid, procStart: processLabel(process.pid), sessionId: id, status: "idle" }));
  const { runtime } = await f.bridge.updates.acquire();
  let sdkReads = 0;
  runtime.sdk.getSessionMessages = async () => { sdkReads++; throw new Error("A task badge must not read SDK history"); };
  const read = async () => (await f.bridge.request("thread/backgroundTasks/list", { threadId: thread.id, runningElsewhereOnly: true })).data;
  assert.deepEqual((await read()).map(t => [t.id, t.status, t.runningElsewhere]), [["bg", "running", true]]);
  assert.equal(sdkReads, 0);
  const listStatus = async () => (await f.bridge.request("thread/backgroundTasks/list", { threadId: thread.id, runningOnly: true })).data;
  assert.deepEqual((await listStatus()).map(t => [t.id, t.status]), [["bg", "running"]]);
  const indexed = f.bridge.sessionFiles.files.get(path);
  assert.ok(indexed.bytes < 16 * 1024, "large tool output is not retained in the metadata index");
  assert.equal(await f.bridge.sessionFiles.latestMessageUuid(id, f.primary), "a2");
  assert.equal((await f.bridge.sessionFiles.latestSettings(id, f.primary)).model, "claude-sonnet-4-6");
  // Output completion and process exit change status without a transcript
  // append. The idle process record alone never proves there are no tasks.
  f.bridge.claudeTasksRoot = join(f.root, "task-output");
  const output = join(f.bridge.claudeTasksRoot, projectFolderName(f.primary), id, "tasks");
  await mkdir(output, { recursive: true });
  await writeFile(join(output, "build.output"), "finished\n[exited with code 2]\n");
  assert.equal((await read())[0].status, "failed");
  await writeFile(join(output, "build.output"), "[killed]\n");
  assert.equal((await read())[0].status, "interrupted");
  await rm(join(output, "build.output"));
  assert.equal((await read())[0].status, "running");
  const record = join(records, `${process.pid}.json`);
  const live = JSON.stringify({ pid: process.pid, procStart: processLabel(process.pid), sessionId: id, status: "idle" });
  await rm(record);
  assert.deepEqual(await read(), [], "process exit is observed without a transcript append");
  assert.deepEqual(await listStatus(), [], "a closed unowned chat needs no metadata for its list summary");
  await writeFile(record, live);
  const session = f.sessions.get(thread.id);
  const helpers = f.bridge.sessionFiles.agentTasks.bind(f.bridge.sessionFiles);
  const helperEntered = Promise.withResolvers(), helperRelease = Promise.withResolvers();
  f.bridge.sessionFiles.agentTasks = async (...args) => { helperEntered.resolve(); await helperRelease.promise; return helpers(...args); };
  const beforeRewind = read();
  const oldRejected = assert.rejects(beforeRewind, /updating this conversation/);
  await helperEntered.promise;
  session.rewind = { turnId: "u1" };
  helperRelease.resolve();
  await oldRejected;
  f.bridge.sessionFiles.agentTasks = helpers;
  assert.deepEqual(await read(), [], "a read spanning rewind cannot poison the new task projection, even for an image-only prompt");
  delete session.rewind;
  // A partial terminal notice is not verified completion.
  const notice = line({ type: "user", uuid: "partial", parentUuid: "a2", origin: { kind: "task-notification" }, message: { role: "user", content:
    "<task-notification><task-id>build</task-id><tool-use-id>bg</tool-use-id><status>completed</status></task-notification>" } });
  await appendFile(path, notice.slice(0, -1));
  await assert.rejects(read(), /updating this conversation/);
  await appendFile(path, "\n");
  assert.equal((await read())[0].status, "completed");
  await appendFile(path, line({ type: "user", uuid: "r2", parentUuid: "partial", origin: { kind: "task-notification" }, message: { role: "user", content:
    "<task-notification><task-id>build</task-id><tool-use-id>bg</tool-use-id><status>completed</status><summary>Done</summary></task-notification>" } }));
  assert.equal((await read())[0].status, "completed", "an append updates the task without SDK history");
  assert.deepEqual(await listStatus(), [], "completed tasks do not mark a list row");
  assert.equal(f.bridge.sessionFiles.files.get(path), indexed, "append advances the existing index");
  assert.equal(sdkReads, 0);
  // A caller timing out cannot enqueue another native metadata read. The
  // pending operation remains the owner until it ends, including an update.
  const metadata = f.bridge.sessionFiles.taskMessages.bind(f.bridge.sessionFiles);
  const entered = Promise.withResolvers(), release = Promise.withResolvers();
  let metadataReads = 0;
  f.bridge.sessionFiles.taskMessages = async (...args) => { metadataReads++; entered.resolve(); await release.promise; return metadata(...args); };
  const first = f.bridge.projectTaskMessages(session);
  await entered.promise;
  const second = f.bridge.projectTaskMessages(session);
  const rejected = [first, second].map(reading => assert.rejects(reading, /updating this conversation/));
  assert.equal(metadataReads, 1, "a second caller joins the pending refresh");
  await f.bridge.runtimeActivated({ version: "new-task-reader" });
  release.resolve();
  await Promise.all(rejected);
  assert.equal(metadataReads, 1);
  f.bridge.sessionFiles.taskMessages = metadata;
  assert.equal((await read())[0].status, "completed", "a fresh epoch can verify the task again");
});

test("task metadata follows branches, compaction links and parallel tool siblings", async t => {
  const root = await mkdtemp(join(tmpdir(), "claude-task-chain-"));
  const files = new ClaudeSessionFiles(root), cwd = "/work/Chain", id = "21111111-2222-4333-8444-555555555551";
  t.after(async () => { await files.close(); await rm(root, { recursive: true, force: true }); });
  const folder = join(root, projectFolderName(cwd)), path = join(folder, `${id}.jsonl`);
  await mkdir(folder, { recursive: true });
  const launch = (uuid, parentUuid, messageId, tool) => ({ type: "assistant", uuid, parentUuid,
    message: { id: messageId, content: [{ type: "tool_use", id: tool, name: "Bash", input: { command: tool, run_in_background: true } }] } });
  const result = (uuid, parentUuid, tool) => ({ type: "user", uuid, parentUuid,
    message: { content: [{ type: "tool_result", tool_use_id: tool, content: `Command running in background with ID: ${tool}` }] } });
  const rows = [{ type: "user", uuid: "u1", parentUuid: null, message: { content: "Build" } },
    launch("a1", "u1", "parallel", "one"), launch("a2", "u1", "parallel", "two"),
    result("r1", "a1", "one"), result("r2", "a2", "two")];
  const encode = values => values.map(row => JSON.stringify({ sessionId: id, ...row })).join("\n") + "\n";
  await writeFile(path, encode(rows));
  const tasks = async () => backgroundTasks(await files.taskMessages(id, cwd), true).map(t => t.id).sort();
  assert.deepEqual(await tasks(), ["one", "two"]);
  await appendFile(path, encode([
    { type: "system", subtype: "compact_boundary", uuid: "compact", parentUuid: null, logicalParentUuid: "r2" },
    { type: "user", uuid: "summary", parentUuid: "compact", isCompactSummary: true, message: { content: "Summary" } },
    { type: "assistant", uuid: "done", parentUuid: "summary", message: { id: "after", stop_reason: "end_turn", content: [] } },
  ]));
  assert.deepEqual(await tasks(), ["one", "two"], "compaction keeps launched tasks");
  await appendFile(path, encode([launch("branch", "u1", "other", "three"), result("rb", "branch", "three")]));
  assert.deepEqual(await tasks(), ["three"], "the current branch excludes abandoned work");
  // Native compaction can reattach preserved messages without a logical
  // parent. Their launches/results still belong to the selected conversation.
  for (const preserved of [
    { preservedMessages: { anchorUuid: "compact", uuids: ["a1", "a2", "r1", "r2"] } },
    { preservedSegment: { anchorUuid: "compact", headUuid: "a2", tailUuid: "r2" } },
  ]) {
    await writeFile(path, encode([...rows,
      { type: "system", subtype: "compact_boundary", uuid: "compact", parentUuid: null, compactMetadata: preserved },
      { type: "user", uuid: "summary", parentUuid: "compact", isCompactSummary: true, message: { content: "Summary" } },
      { type: "assistant", uuid: "end", parentUuid: "summary", message: { id: "end", stop_reason: "end_turn", content: [] } },
    ]));
    assert.deepEqual(await tasks(), ["one", "two"], "preserved native messages survive compaction");
  }
  const replacement = [rows[0], launch("new", "u1", "new", "replacement"), result("rn", "new", "replacement")];
  await writeFile(path, encode(replacement));
  assert.deepEqual(await tasks(), ["replacement"], "replacement discards old indexed tasks");
});

// The fake SDK does not write Claude Code's transcript. Task-status reads
// now need that native input too: mirror these synthetic messages into real
// JSONL while retaining explicit branch links and avoiding unchanged rewrites.
function syntheticTaskFiles(bridge, source) {
  bridge.sessionFiles.root = join(bridge.sessions.root, "native-task-fixture");
  const read = bridge.readProjectTaskMessages.bind(bridge), written = new Map();
  bridge.readProjectTaskMessages = async session => {
    const messages = await source(session.sdkSessionId);
    if (messages) {
      const folder = join(bridge.sessionFiles.root, projectFolderName(session.options.wonderProject.cwd));
      let parent = null;
      const text = messages.map((message, i) => {
        const uuid = message.uuid ?? `fixture-${i}`;
        const row = { sessionId: session.sdkSessionId, parentUuid: parent, ...message, uuid,
          ...(message.is_meta ? { isMeta: true } : {}), ...(message.tool_use_result ? { toolUseResult: message.tool_use_result } : {}) };
        parent = uuid;
        return JSON.stringify(row);
      }).join("\n") + "\n";
      const path = join(folder, `${session.sdkSessionId}.jsonl`);
      if (written.get(path) !== text) { await mkdir(folder, { recursive: true }); await writeFile(path, text); written.set(path, text); }
    }
    return read(session);
  };
}

// Contract owner: project conversations continue the owner's native Claude Code
// session by exact UUID, read history from that transcript, and never receive
// unadvertised tools.
async function fixture(t, { transcripts = {}, claudeSessionsDir = undefined, leftoverResult = true, nativeTaskFiles = false } = {}) {
  const gate = { current: null };
  const root = await mkdtemp(join(tmpdir(), "wonder-project-"));
  let bridge;
  t.after(async () => { await bridge?.close(); await rm(root, { recursive: true, force: true }); });
  const primary = join(root, "app"), secondary = join(root, "docs");
  await mkdir(primary); await mkdir(secondary);
  const sessions = await new Sessions(join(root, "sessions")).initialize();
  const frames = [], inputs = [], inputSettings = [], capturedOptions = [], interrupts = [], closes = [], rewinds = [], promptContexts = [];
  // Permission modes set on a running process (`Query.setPermissionMode`), with how many prompts it had then; a queued answer holds the change.
  const modeChanges = [], modeAnswers = [];
  // What Claude's file restore answers (`Query.rewindFiles`); the fake never touches files.
  const rewindFiles = { current: (_id, { dryRun = false } = {}) => ({ canRewind: true, ...(dryRun ? {} : { filesChanged: ["a.txt"], insertions: 1, deletions: 1 }) }) };
  const lingers = { current: false }; // The fake process stays up after a reply, as Claude Code does.
  const emitQueue = [], emitWaiters = [];
  const emits = { push: message => (emitWaiters.shift() ?? (m => emitQueue.push(m)))(message),
    next: () => emitQueue.length ? Promise.resolve(emitQueue.shift()) : new Promise(resolve => emitWaiters.push(resolve)) };
  const sdk = {
    tool: (name, _description, _schema, handler) => ({ name, handler }), createSdkMcpServer: ({ tools }) => ({ tools }),
    listSessions: async ({ dir, offset = 0, limit = Infinity }) => Object.entries(transcripts).filter(([, s]) => s.cwd === dir)
      .map(([sessionId, s]) => ({ sessionId, summary: s.title, lastModified: s.updatedAt, cwd: s.cwd }))
      .sort((a, b) => b.lastModified - a.lastModified || b.sessionId.localeCompare(a.sessionId)).slice(offset, offset + limit),
    getSessionInfo: async (id, { dir }) => transcripts[id]?.cwd === dir ? { sessionId: id, cwd: dir, lastModified: 1 } : undefined,
    getSessionMessages: async id => transcripts[id]?.messages ?? [],
    query: ({ prompt, options }) => {
      capturedOptions.push(options);
      let settings = { model: options.model, effortLevel: options.effort ?? null, ...options.settings };
      let interrupted = false, closed = false, nativeSessionId = options.sessionId ?? options.resume;
      const nativeMessage = message => {
        if (((message.type === "system" && message.subtype === "init") || message.type === "result")
          && !message.parent_tool_use_id && message.session_id) nativeSessionId = message.session_id;
        return message;
      };
      const query = (async function* () {
        const prompts = prompt[Symbol.asyncIterator]();
        let nextPrompt = prompts.next(), nextEmit = null;
        for (;;) {
          if (closed) return; // close() ends the Claude process and its output.
          // Messages the process produces on its own, such as an agent finishing.
          nextEmit ??= emits.next();
          const got = await Promise.race([nextPrompt, nextEmit.then(message => ({ emitted: message }))]);
          if (got.emitted) { nextEmit = null; yield nativeMessage(got.emitted); continue; }
          if (got.done) return;
          const input = got.value;
          inputs.push(input); inputSettings.push(structuredClone(settings));
          // Like Claude Code: each prompt runs the UserPromptSubmit hooks, whose
          // additionalContext reaches Claude beside the prompt, not inside it.
          for (const { hooks } of options.hooks?.UserPromptSubmit ?? [])
            for (const hook of hooks) promptContexts.push((await hook({ hook_event_name: "UserPromptSubmit", prompt: "" })).hookSpecificOutput?.additionalContext ?? null);
          yield { type: "system", subtype: "init", session_id: nativeSessionId };
          // While held, the process can still report what an agent does.
          for (let held = gate.current; held;) {
            nextEmit ??= emits.next();
            const event = await Promise.race([held.promise.then(() => null), nextEmit]);
            if (!event) break;
            nextEmit = null; yield nativeMessage(event);
          }
          if (closed) return;
          if (interrupted) { // Like Claude Code: the interrupt result, then the process stays up.
            interrupted = false;
            yield { type: "result", subtype: "error_during_execution", is_error: true, user_message_uuid: input.uuid };
            nextPrompt = prompts.next();
            continue;
          }
          if (leftoverResult) yield { type: "result", subtype: "success" }; // A leftover background notice.
          yield { type: "assistant", user_message_uuid: input.uuid,
            message: { id: "reply", content: [{ type: "text", text: "Answered the new prompt." }] } };
          yield { type: "result", subtype: "success", user_message_uuid: input.uuid };
          if (!lingers.current) return;
          nextPrompt = prompts.next();
        }
      })();
      query.initializationResult = async () => ({});
      query.accountInfo = async () => ({ apiProvider: "firstParty", subscriptionType: "Claude Max" });
      query.applyFlagSettings = async next => { settings = { ...settings, ...next }; };
      query.close = () => { closes.push(1); closed = true; gate.current?.resolve(); };
      query.interrupt = async () => { interrupts.push(1); interrupted = true; gate.current?.resolve(); };
      query.rewindFiles = async (id, opts) => { rewinds.push({ id, opts: opts ?? {}, options }); return rewindFiles.current(id, opts); };
      query.setPermissionMode = async mode => { modeChanges.push({ mode, prompts: inputs.length }); await modeAnswers.shift(); };
      return query;
    },
  };
  const updates = { acquire: async () => ({ runtime: { sdk }, release: async () => {} }) };
  bridge = new ClaudeBridge({ updates, sessions, send: async frame => { frames.push(frame); }, claudeSessionsDir: claudeSessionsDir ?? join(root, "native-sessions"), yieldCheckMs: 20,
    claudeProjectsDir: join(root, "claude-projects") });
  if (!nativeTaskFiles) syntheticTaskFiles(bridge, id => transcripts[id]?.messages ?? []);
  const policy = { mode: "workspace", approvalMode: "ask", readRoots: ["/"], writeRoots: [primary, secondary], deniedRoots: [] };
  const project = { cwd: primary, additionalDirectories: [secondary] };
  return { root, primary, secondary, sessions, bridge, frames, inputs, inputSettings, capturedOptions, interrupts, closes, lingers, emits, policy, project, gate, rewinds, rewindFiles, promptContexts, modeChanges, modeAnswers };
}

test("the chosen speed and ultracode reach the SDK query options", async t => {
  const f = await fixture(t);
  const { thread } = await f.bridge.request("thread/start", { cwd: f.primary, model: "claude:opus",
    wonderPolicy: f.policy, wonderProject: f.project, dynamicTools: [] });
  await f.bridge.request("turn/start", { threadId: thread.id, effort: "ultracode", serviceTier: "fast",
    input: [{ type: "text", text: "Go fast" }] });
  await new Promise(resolve => setImmediate(resolve));
  await f.bridge.active.get(thread.id)?.finished;
  const options = f.capturedOptions[0];
  assert.equal(options.settings.fastMode, true);
  assert.equal(options.settings.ultracode, true);
  assert.equal(options.effort, "max", "ultracode runs on the highest effort");
});

test("project turns use the native coding preset and name the transcript entry after the Cupola turn", async t => {
  const f = await fixture(t);
  const { thread } = await f.bridge.request("thread/start", { cwd: f.primary, model: "claude:haiku",
    wonderPolicy: f.policy, wonderProject: f.project, dynamicTools: [{ name: "wonder_unknown" }] });
  const { turn } = await f.bridge.request("turn/start", { threadId: thread.id, input: [{ type: "text", text: "Fix the build" }] });
  await new Promise(resolve => setImmediate(resolve));
  await f.bridge.active.get(thread.id)?.finished;
  const options = f.capturedOptions[0];
  // Projects run without a sandbox, so there is no sandbox guidance to add.
  assert.deepEqual(options.systemPrompt, { type: "preset", preset: "claude_code", append: NESTED_SANDBOX_GUIDANCE });
  assert.equal(options.sandbox.enabled, true); assert.equal(options.sandbox.allowUnsandboxedCommands, false);
  assert.deepEqual(options.settingSources, ["user", "project", "local"]);
  assert.deepEqual(options.additionalDirectories, [f.secondary]);
  assert.deepEqual(options.mcpServers, {});
  // Projects offer a per-task Stop, so a turn interrupt may spare background agents.
  assert.equal(options.perTaskStopAffordance, true);
  assert.equal(options.agentProgressSummaries, true);
  assert.equal(options.sessionId, thread.sessionId);
  assert.equal(f.inputs[0].uuid, turn.id);
  const saved = f.sessions.get(thread.id).turns[0];
  assert.equal(saved.status, "completed");
  assert.ok(saved.items.some(item => item.type === "agentMessage" && item.text === "Answered the new prompt."));
  // A project cannot be moved to another folder by a later turn.
  await assert.rejects(f.bridge.request("turn/start", { threadId: thread.id, input: [{ type: "text", text: "x" }],
    wonderProject: { cwd: f.secondary } }), /keeps its working folder/);
});

// Contract: a Project turn offers Claude the daemon's thread tools under the
// `wonder` server, and nothing else the daemon happens to register; a call
// round-trips through the daemon only after its permission check.
test("project turns expose only the thread tools and route them through the daemon", async t => {
  const f = await fixture(t);
  const { names } = JSON.parse(await readFile(new URL("../../../packages/protocol/thread-tools.json", import.meta.url), "utf8"));
  const spec = name => ({ name, description: name, inputSchema: { type: "object", properties: { limit: { type: "integer" } } } });
  const { thread } = await f.bridge.request("thread/start", { cwd: f.primary, model: "claude:haiku", wonderPolicy: f.policy, wonderProject: f.project });
  f.gate.current = Promise.withResolvers();
  await f.bridge.request("turn/start", { threadId: thread.id, input: [{ type: "text", text: "Delegate it" }],
    dynamicTools: [...names.map(spec), spec("wonder_ask_question"), spec("wonder_computer_use")] });
  while (!f.capturedOptions.length) await new Promise(resolve => setImmediate(resolve));
  const options = f.capturedOptions[0];
  assert.deepEqual(options.mcpServers.wonder.tools.map(tool => tool.name), names);
  const list = options.mcpServers.wonder.tools[0].handler;
  const origin = { name: "wonder", source: "sdk" };
  assert.equal((await options.canUseTool("mcp__wonder__wonder_thread_list", {}, { toolUseID: "forged", mcpServer: { name: "wonder", source: "user" } })).behavior, "deny");
  assert.equal((await options.canUseTool("mcp__wonder__wonder_thread_list", { limit: 5 }, { toolUseID: "call-1", mcpServer: origin })).behavior, "allow");
  const answered = list({ limit: 5 });
  await new Promise(resolve => setImmediate(resolve));
  const call = f.frames.find(frame => frame.method === "item/tool/call");
  assert.equal(call.params.tool, "wonder_thread_list");
  assert.equal(call.params.callId, "call-1");
  assert.deepEqual(call.params.arguments, { limit: 5 });
  await f.bridge.receive({ id: call.id, result: { success: true, contentItems: [{ type: "inputText", text: '{"threads":[]}' }] } });
  assert.equal((await answered).isError, false);
  f.gate.current.resolve();
  await f.bridge.active.get(thread.id)?.finished;
});

// Contract: the owner's approval and plan modes reach every turn as Claude
// Code's own permission mode, with no sandbox. Whatever Claude Code would ask
// reaches the owner; Full access never asks; Read only keeps a sandbox whose
// new hosts ask; and planning refuses writes and proposals to continue.
test("project approval and plan modes reach the SDK and Claude Code's prompts reach the owner", async t => {
  const f = await fixture(t);
  await assert.rejects(f.bridge.request("thread/start", { cwd: f.primary, model: "claude:haiku", wonderProject: f.project,
    wonderPolicy: { ...f.policy, mode: "read_only", approvalMode: "auto" } }), /unsupported/);
  const { thread } = await f.bridge.request("thread/start", { cwd: f.primary, model: "claude:haiku", wonderPolicy: f.policy, wonderProject: f.project });
  const settle = () => new Promise(resolve => setImmediate(resolve));
  const turn = async (modes, body) => {
    f.gate.current = Promise.withResolvers();
    const before = f.capturedOptions.length;
    await f.bridge.request("turn/start", { threadId: thread.id, input: [{ type: "text", text: "Go" }],
      wonderPolicy: { ...f.policy, ...modes }, wonderProject: f.project });
    while (f.capturedOptions.length === before) await settle();
    const options = f.capturedOptions.at(-1);
    const hook = (tool_name, tool_input) => options.hooks.PreToolUse[0].hooks[0]({ tool_name, tool_input }).then(r => r.hookSpecificOutput);
    // Resolves the next approval request the owner would see.
    const approve = async (call, decision, check = () => {}) => {
      await settle();
      const request = f.frames.at(-1);
      assert.equal(request.method, "item/commandExecution/requestApproval");
      check(request.params);
      await f.bridge.receive({ id: request.id, result: { decision } });
      return call;
    };
    try { await body({ options, hook, approve }); }
    finally { f.gate.current.resolve(); await f.bridge.active.get(thread.id)?.finished; }
  };
  const inside = { file_path: join(f.primary, "a.txt"), content: "x" }, ctx = id => ({ toolUseID: id });
  const ran = (options, result) => {
    assert.equal(options.sandbox.enabled, true); assert.equal(options.sandbox.allowUnsandboxedCommands, false);
    assert.deepEqual(result, { behavior: "allow", updatedInput: { command: "echo hi" } });
  };
  const outside = { command: "git push", dangerouslyDisableSandbox: true };
  const network = (host, decision) => async ({ options, approve }) => {
    const call = options.canUseTool("SandboxNetworkAccess", { host }, ctx(`net-${host}`));
    return approve(call, decision, params => {
      assert.deepEqual(params.networkApprovalContext, { host, protocol: "https" });
      assert.ok(params.availableDecisions.some(d => d?.applyNetworkPolicyAmendment?.network_policy_amendment?.host === host));
    });
  };

  await turn({ approvalMode: "auto" }, async ({ options, hook, approve }) => {
    // Claude Code's classifier approves routine commands; an escalation asks the owner.
    assert.equal(options.permissionMode, "auto");
    assert.deepEqual(await hook("Bash", { command: "echo hi" }), undefined);
    ran(options, await approve(options.canUseTool("Bash", { command: "echo hi" }, ctx("auto-bash")), "accept",
      params => assert.equal(params.command, "echo hi")));
    const declined = await approve(options.canUseTool("Bash", { command: "git push" }, ctx("auto-push")), "decline");
    assert.equal(declined.behavior, "deny");
    assert.match(declined.message, /owner declined/);
    assert.equal((await hook("Write", inside)).permissionDecision, "allow");
    assert.equal((await options.canUseTool("Write", inside, ctx("auto-write"))).behavior, "allow");
    const web = options.canUseTool("WebFetch", { url: "https://example.com" }, ctx("auto-web"));
    assert.equal((await approve(web, "decline")).behavior, "deny");
  });
  await turn({ mode: "read_only", approvalMode: "ask", writeRoots: [] }, async ({ options, approve }) => {
    assert.equal(options.sandbox.enabled, true);
    // Leaving Read only's sandbox is the owner's decision.
    const declined = await approve(options.canUseTool("Bash", outside, ctx("ro-outside")), "decline",
      params => assert.match(params.reason, /outside the sandbox/));
    assert.match(declined.message, /outside the sandbox/);
    // A new host is allowed once, for the session, or saved to Claude Code's project settings.
    assert.deepEqual(await network("example.com", "accept")({ options, approve }), { behavior: "allow", updatedInput: { host: "example.com" } });
    assert.equal((await network("example.org", "acceptForSession")({ options, approve })).updatedPermissions[0].destination, "session");
    const saved = await network("example.net", { applyNetworkPolicyAmendment: { network_policy_amendment: { host: "example.net", action: "allow" } } })({ options, approve });
    assert.deepEqual(saved.updatedPermissions, [{ type: "addRules", rules: [{ toolName: "WebFetch", ruleContent: "domain:example.net" }], behavior: "allow", destination: "localSettings" }]);
    const refused = await network("evil.example", "decline")({ options, approve });
    assert.equal(refused.behavior, "deny");
    assert.match(refused.message, /did not allow evil\.example/);
  });
  await turn({ approvalMode: "accept_edits" }, async ({ options, approve }) => {
    assert.equal(options.permissionMode, "default");
    assert.equal((await options.canUseTool("Edit", inside, ctx("edit"))).behavior, "allow");
    ran(options, await approve(options.canUseTool("Bash", { command: "echo hi" }, ctx("ask-bash")), "accept"));
  });
  await turn({ approvalMode: "auto", planMode: true }, async ({ options, hook, approve }) => {
    assert.equal(options.permissionMode, "plan");
    assert.ok(options.tools.includes("ExitPlanMode"));
    for (const tool of ["Write", "Edit"]) {
      assert.equal((await hook(tool, inside)).permissionDecision, "deny");
      const denied = await options.canUseTool(tool, inside, ctx(`plan-${tool}`));
      assert.equal(denied.behavior, "deny");
      assert.match(denied.message, /Plan mode: describe the change instead of editing/);
    }
    const exit = await options.canUseTool("ExitPlanMode", { plan: "# Plan" }, ctx("plan-exit"));
    assert.equal(exit.behavior, "deny");
    assert.match(exit.message, /owner reviews the plan in Cupola/);
    // Even in Auto, planning asks before running a command.
    ran(options, await approve(options.canUseTool("Bash", { command: "echo hi" }, ctx("plan-bash")), "accept"));
  });
  await turn({ approvalMode: "ask" }, async ({ options }) => {
    assert.equal(options.permissionMode, "default");
    assert.ok(!options.tools.includes("ExitPlanMode"));
  });
  // Full access means no questions; commands are still sandboxed and never leave it.
  await turn({ mode: "full_access", approvalMode: "full_access" }, async ({ options }) => {
    assert.equal(options.sandbox.enabled, true); assert.equal(options.sandbox.allowUnsandboxedCommands, false);
    assert.deepEqual(options.systemPrompt, { type: "preset", preset: "claude_code", append: NESTED_SANDBOX_GUIDANCE });
    const before = f.frames.length;
    assert.equal((await options.canUseTool("Bash", { command: "ls" }, ctx("full-bash"))).behavior, "allow");
    assert.equal(f.frames.length, before);
  });
});

// Contract (owner, Oct 25): the owner's claude.ai connectors that Cupola lists
// as usable load in Cupola's Claude turns, and each call follows the chat's
// access: Ask and Auto ask the owner per call, Full access does not.
test("connectors listed as usable load in project turns and each call follows the chat's access", async t => {
  const f = await fixture(t);
  f.bridge.inspect = async (_runtime, { connectors }) => ({ servers: connectors
    ? [{ name: "claude.ai Gmail", source: "claudeai", status: "connected" }] : [] });
  const { apps } = await f.bridge.request("app/installed", {});
  assert.deepEqual(apps.map(app => [app.name, app.callable, app.isAccessible]), [["Gmail", true, true]]);
  const { thread } = await f.bridge.request("thread/start", { cwd: f.primary, model: "claude:haiku", wonderPolicy: f.policy, wonderProject: f.project });
  const settle = () => new Promise(resolve => setImmediate(resolve));
  const gmail = { toolUseID: "", mcpServer: { name: "claude.ai Gmail", source: "claudeai" } };
  for (const [modes, asks] of [[{ approvalMode: "ask" }, true], [{ approvalMode: "auto" }, true],
    [{ mode: "full_access", approvalMode: "full_access" }, false]]) {
    f.gate.current = Promise.withResolvers();
    const before = f.capturedOptions.length;
    await f.bridge.request("turn/start", { threadId: thread.id, input: [{ type: "text", text: "Find the invoice" }],
      wonderPolicy: { ...f.policy, ...modes }, wonderProject: f.project });
    while (f.capturedOptions.length === before) await settle();
    const options = f.capturedOptions.at(-1);
    try {
      assert.equal(options.strictMcpConfig, false, modes.approvalMode);
      assert.equal(options.settings.disableClaudeAiConnectors, false, modes.approvalMode);
      const frames = f.frames.length;
      const call = options.canUseTool("mcp__claude_ai_Gmail__search_threads", { query: "invoice" }, { ...gmail, toolUseID: `gmail-${modes.approvalMode}` });
      await settle();
      if (asks) {
        const request = f.frames.at(-1);
        assert.equal(request.method, "item/commandExecution/requestApproval");
        assert.match(request.params.command, /^mcp__claude_ai_Gmail__search_threads: /);
        await f.bridge.receive({ id: request.id, result: { decision: "decline" } });
        assert.equal((await call).behavior, "deny");
      } else {
        assert.equal((await call).behavior, "allow");
        assert.equal(f.frames.length, frames);
      }
    } finally { f.gate.current.resolve(); await f.bridge.active.get(thread.id)?.finished; }
  }
});

// Contract owner: skills. Every process has the Skill tool and Claude Code
// discovers the owner's own skills itself; Cupola lists none. Cupola's pack
// plugin is added only while one of its skills is on, and a Wonder skill that
// is off is refused. Once one of the owner's skills or a typed `/name` loads,
// commands and web access stop deferring to Claude Code's rules (where a
// skill's `allowed-tools` would apply) and reach the owner instead.
test("the owner's own skills load in every process and Cupola's only while on", async t => {
  const f = await fixture(t);
  const { thread } = await f.bridge.request("thread/start", { cwd: f.primary, model: "claude:haiku",
    wonderPolicy: f.policy, wonderProject: f.project });
  const settle = () => new Promise(resolve => setImmediate(resolve));
  const plugin = join(f.root, "agent-skills");
  const pack = { plugin, skills: ["wonder:wonder-phone-replies", "wonder:wonder-share-to-phone"] };
  const turn = async (extra, body, text = "Hi") => {
    f.gate.current = Promise.withResolvers();
    const before = f.capturedOptions.length;
    await f.bridge.request("turn/start", { threadId: thread.id, input: [{ type: "text", text }],
      wonderPolicy: { ...f.policy, approvalMode: "auto" }, wonderProject: f.project, ...extra });
    while (f.capturedOptions.length === before) await settle();
    const options = f.capturedOptions.at(-1);
    const hook = (tool_name, tool_input) => options.hooks.PreToolUse[0].hooks[0]({ hook_event_name: "PreToolUse", tool_name, tool_input })
      .then(r => r.hookSpecificOutput);
    const decide = async skill => (await hook("Skill", { skill })).permissionDecision;
    try { await body({ options, hook, decide }); }
    finally { f.gate.current.resolve(); await f.bridge.active.get(thread.id)?.finished; }
  };
  let off;
  await turn({}, async ({ options, hook, decide }) => {
    off = options;
    assert.deepEqual(options.plugins, []);
    assert.ok(!("skills" in options));
    assert.ok(options.tools.includes("Skill"));
    assert.deepEqual(options.settingSources, ["user", "project", "local"]);
    assert.equal(await decide("wonder:wonder-phone-replies"), "deny");
    // Until a skill of the owner's loads, Auto's routine commands are Claude Code's call.
    assert.equal(await hook("Bash", { command: "echo hi" }), undefined);
    assert.equal(await decide("deploy"), "allow");
    assert.equal(await decide("other:review"), "allow");
    // Bash inside a skill still asks the owner, whatever the skill allows.
    assert.equal((await hook("Bash", { command: "echo hi" })).permissionDecision, "ask");
    assert.equal((await hook("WebFetch", { url: "https://example.com" })).permissionDecision, "ask");
    const call = options.canUseTool("Bash", { command: "echo hi" }, { toolUseID: "skill-bash" });
    await settle();
    const request = f.frames.at(-1);
    assert.equal(request.method, "item/commandExecution/requestApproval");
    assert.equal(request.params.command, "echo hi");
    await f.bridge.receive({ id: request.id, result: { decision: "decline" } });
    assert.equal((await call).behavior, "deny");
  });
  await turn({ wonderSkills: pack }, async ({ options, hook, decide }) => {
    assert.deepEqual(options.plugins, [{ type: "local", path: plugin, skipMcpDiscovery: true }]);
    assert.ok(!("skills" in options));
    assert.deepEqual(options.tools, off.tools);
    assert.deepEqual(options.settingSources, off.settingSources);
    assert.deepEqual(options.systemPrompt, off.systemPrompt);
    assert.equal(await decide("wonder:wonder-phone-replies"), "allow");
    assert.equal(await decide("wonder-share-to-phone"), "allow");
    assert.equal(await decide("wonder:wonder-phone-questions"), "deny");
    // Cupola's own skills name no tools, so nothing changes for commands.
    assert.equal(await hook("Bash", { command: "echo hi" }), undefined);
    assert.equal(await decide("deploy"), "allow");
  });
  // A typed `/name` runs its skill with the tools it names: commands ask from the start.
  await turn({}, async ({ hook }) => {
    assert.equal((await hook("Bash", { command: "echo hi" })).permissionDecision, "ask");
  }, "/deploy now");
  await assert.rejects(f.bridge.request("turn/start", { threadId: thread.id, input: [{ type: "text", text: "x" }],
    wonderSkills: { plugin, skills: ["deploy"] } }), /skills are invalid/);
});

// A skill's tool grants last as long as Claude Code's process, which can
// serve later replies: those replies keep asking for commands too.
test("a skill loaded in a kept process keeps commands asking in its next reply", async t => {
  const f = await fixture(t);
  f.lingers.current = true;
  const { thread } = await f.bridge.request("thread/start", { cwd: f.primary, model: "claude:haiku",
    wonderPolicy: { ...f.policy, approvalMode: "auto" }, wonderProject: f.project });
  const until = async ready => { while (!ready()) await new Promise(resolve => setImmediate(resolve)); };
  const reply = async (text, count) => {
    await f.bridge.request("turn/start", { threadId: thread.id, input: [{ type: "text", text }] });
    await until(() => f.inputs.length === count);
    await f.bridge.active.get(thread.id)?.finished;
  };
  await reply("Hi", 1);
  const hook = (tool_name, tool_input) => f.capturedOptions[0].hooks.PreToolUse[0].hooks[0]({ hook_event_name: "PreToolUse", tool_name, tool_input })
    .then(r => r.hookSpecificOutput);
  assert.equal(await hook("Bash", { command: "echo hi" }), undefined);
  assert.equal((await hook("Skill", { skill: "deploy" })).permissionDecision, "allow");
  await reply("Again", 2);
  assert.equal(f.capturedOptions.length, 1, "the kept process served the reply");
  assert.equal((await hook("Bash", { command: "echo hi" })).permissionDecision, "ask");
});

// Contract owner: the owner's custom instruction. Without `wonderInstruction`
// a process gets exactly the system prompt it always had; with it, the text is
// appended after Cupola's own guidance, and an invalid one is refused.
test("a custom instruction is appended to the system prompt only when the turn carries it", async t => {
  const f = await fixture(t);
  const { thread } = await f.bridge.request("thread/start", { cwd: f.primary, model: "claude:haiku",
    wonderPolicy: f.policy, wonderProject: f.project });
  const reply = async (extra) => {
    const before = f.capturedOptions.length;
    await f.bridge.request("turn/start", { threadId: thread.id, input: [{ type: "text", text: "Hi" }], ...extra });
    while (f.capturedOptions.length === before) await new Promise(resolve => setImmediate(resolve));
    await f.bridge.active.get(thread.id)?.finished;
    return f.capturedOptions.at(-1);
  };
  const off = await reply({});
  const on = await reply({ wonderInstruction: "Answer in British English." });
  const appended = on.systemPrompt.append.split("\n\n");
  assert.equal(appended.at(-1), "Answer in British English.");
  assert.deepEqual({ ...on.systemPrompt, append: appended.slice(0, -1).join("\n\n") || undefined },
    { ...off.systemPrompt, append: off.systemPrompt.append });
  assert.deepEqual(on.tools, off.tools);
  assert.deepEqual(on.settingSources, off.settingSources);
  assert.deepEqual(on.plugins, off.plugins);
  const again = await reply({});
  assert.deepEqual(again.systemPrompt, off.systemPrompt);
  for (const wonderInstruction of ["   ", 7, "x".repeat(4001)])
    await assert.rejects(f.bridge.request("turn/start", { threadId: thread.id, input: [{ type: "text", text: "x" }], wonderInstruction }),
      /custom instruction for Claude is invalid/);
});

// Contract owner: Cupola's preamble. A turn carrying `wonderPreamble` gets it
// first in the system-prompt append, before Cupola's sandbox guidance and the
// owner's instruction; without it the prompt is as before, and an invalid one
// is refused.
test("Cupola's preamble opens the system-prompt append only when the turn carries it", async t => {
  const f = await fixture(t);
  const { thread } = await f.bridge.request("thread/start", { cwd: f.primary, model: "claude:haiku",
    wonderPolicy: f.policy, wonderProject: f.project });
  const reply = async (extra) => {
    const before = f.capturedOptions.length;
    await f.bridge.request("turn/start", { threadId: thread.id, input: [{ type: "text", text: "Hi" }], ...extra });
    while (f.capturedOptions.length === before) await new Promise(resolve => setImmediate(resolve));
    await f.bridge.active.get(thread.id)?.finished;
    return f.capturedOptions.at(-1);
  };
  const off = await reply({});
  const preamble = "This conversation runs in Cupola.";
  const on = await reply({ wonderPreamble: preamble, wonderInstruction: "Be brief." });
  const appended = on.systemPrompt.append.split("\n\n");
  assert.equal(appended[0], preamble);
  assert.equal(appended.at(-1), "Be brief.");
  assert.equal(appended.slice(1, -1).join("\n\n") || undefined, off.systemPrompt.append);
  for (const wonderPreamble of ["   ", 7, "x".repeat(2001)])
    await assert.rejects(f.bridge.request("turn/start", { threadId: thread.id, input: [{ type: "text", text: "x" }], wonderPreamble }),
      /Cupola couldn't prepare this message for Claude\. Restart Cupola on your computer, then send it again\.$/);
});

// Stop while the kept process is still changing its permission mode ends
// that reply before its message reaches Claude.
test("a reply stopped while the kept process changes permission mode never reaches Claude", async t => {
  const f = await fixture(t);
  f.lingers.current = true;
  const { thread } = await f.bridge.request("thread/start", { cwd: f.primary, model: "claude:haiku",
    wonderPolicy: f.policy, wonderProject: f.project });
  const until = async ready => { while (!ready()) await new Promise(resolve => setImmediate(resolve)); };
  await f.bridge.request("turn/start", { threadId: thread.id, input: [{ type: "text", text: "Hi" }] });
  await until(() => f.inputs.length === 1);
  await f.bridge.active.get(thread.id)?.finished;
  const held = Promise.withResolvers();
  f.modeAnswers.push(held.promise);
  const { turn } = await f.bridge.request("turn/start", { threadId: thread.id, input: [{ type: "text", text: "Stopped" }],
    wonderPolicy: { ...f.policy, approvalMode: "auto" } });
  await until(() => f.modeChanges.length === 1);
  await f.bridge.request("turn/interrupt", { threadId: thread.id, turnId: turn.id });
  held.resolve();
  await f.bridge.active.get(thread.id)?.finished;
  assert.deepEqual(f.modeChanges, [{ mode: "auto", prompts: 1 }]);
  assert.equal(f.capturedOptions.length, 1, "the kept process took the change");
  assert.deepEqual(f.inputs.map(input => input.message.content[0].text), ["Hi"], "the stopped message is never sent");
  const ended = f.frames.filter(frame => frame.method === "turn/completed" && frame.params.turn.id === turn.id);
  assert.deepEqual(ended.map(frame => frame.params.turn.status), ["interrupted"]);
});

// Contract owner: host context for one turn (`wonderContext`, the phone
// signal). Claude receives it as UserPromptSubmit additionalContext for that
// prompt only; the message Claude gets is exactly what the owner wrote, the
// system prompt is unchanged and the kept process serves the next reply.
test("turn context reaches Claude beside the owner's message, never inside it", async t => {
  const f = await fixture(t);
  f.lingers.current = true;
  const { thread } = await f.bridge.request("thread/start", { cwd: f.primary, model: "claude:haiku",
    wonderPolicy: f.policy, wonderProject: f.project });
  const reply = async (text, extra) => {
    const before = f.inputs.length;
    await f.bridge.request("turn/start", { threadId: thread.id, input: [{ type: "text", text }], ...extra });
    while (f.inputs.length === before) await new Promise(resolve => setImmediate(resolve));
    await f.bridge.active.get(thread.id)?.finished;
    return f.inputs.at(-1);
  };
  const phone = "The user sent this message from the Cupola app on their phone, away from this computer's screen.";
  const plain = await reply("Hi", {});
  const fromPhone = await reply("Check the build", { wonderContext: [phone] });
  const again = await reply("Thanks", {});
  assert.deepEqual(fromPhone.message.content, [{ type: "text", text: "Check the build" }]);
  assert.deepEqual(plain.message.content, [{ type: "text", text: "Hi" }]);
  assert.deepEqual(again.message.content, [{ type: "text", text: "Thanks" }]);
  assert.deepEqual(f.promptContexts, [null, phone, null]);
  // A prompt Claude Code injects itself leaves the context for the owner's.
  const live = { context: [phone] };
  assert.deepEqual(promptContext(live, { source: "system" }), {});
  assert.equal(promptContext(live, { source: "sdk" }).hookSpecificOutput.additionalContext, phone);
  assert.deepEqual(promptContext(live, {}), {}, "delivered once");
  assert.equal(f.capturedOptions.length, 1, "the kept process serves every reply");
  assert.deepEqual(f.capturedOptions[0].systemPrompt, { type: "preset", preset: "claude_code", append: NESTED_SANDBOX_GUIDANCE });
  for (const wonderContext of ["x", ["   "], [7], ["x".repeat(1001)], ["a", "b", "c", "d", "e"]])
    await assert.rejects(f.bridge.request("turn/start", { threadId: thread.id, input: [{ type: "text", text: "x" }], wonderContext }),
      /Cupola couldn't prepare this message for Claude\. Restart Cupola on your computer, then send it again\.$/);
});

test("attaching resumes the exact native UUID and never invents a missing session", async t => {
  const id = "11111111-2222-4333-8444-555555555555";
  const f = await fixture(t);
  const params = { sessionId: id, model: "claude:haiku", wonderPolicy: f.policy, wonderProject: f.project };
  await assert.rejects(f.bridge.request("project/session/attach", params), /no longer available/);
  assert.equal(f.sessions.values.size, 0);
  const g = await fixture(t, { transcripts: {} });
  g.bridge.withSdk = action => action({ getSessionInfo: async () => ({ sessionId: id, cwd: g.primary, lastModified: 1 }) });
  const attached = await g.bridge.request("project/session/attach", { ...params, wonderPolicy: g.policy, wonderProject: { cwd: g.primary } });
  assert.equal(attached.thread.sessionId, id);
  const again = await g.bridge.request("project/session/attach", { ...params, wonderPolicy: g.policy, wonderProject: { cwd: g.primary } });
  assert.equal(again.thread.id, attached.thread.id);
  assert.equal(g.sessions.get(attached.thread.id).sdkStarted, true);
});

test("listing reads metadata only and keeps exact folders", async t => {
  const f = await fixture(t);
  const transcripts = { a: { cwd: f.primary, title: "Newer", updatedAt: 3000 }, b: { cwd: `${f.primary}-sibling`, title: "Other", updatedAt: 9000 },
    c: { cwd: f.secondary, title: "Docs", updatedAt: 1000 }, d: { cwd: f.primary, title: "Other session", updatedAt: 5000 } };
  const g = await fixture(t, { transcripts });
  Object.values(transcripts).forEach(s => { if (s.cwd === f.primary) s.cwd = g.primary; if (s.cwd === f.secondary) s.cwd = g.secondary; });
  const result = await g.bridge.request("project/sessions/list", { dirs: [g.primary, g.secondary], limit: 10, excludeSessionIds: ["d"] });
  assert.deepEqual(result.data.map(s => s.sessionId), ["a", "c"]);
  assert.equal(g.capturedOptions.length, 0);
});

// Native metadata is a descending stream per folder. The merged catalog must
// continue past excluded windows and 5,000 entries without skipping another
// folder's newer rows or mutating the token used to retry a dropped response.
test("native catalog merges independent offsets and continues after excluded windows", async t => {
  const f = await fixture(t), calls = [];
  const rows = [f.primary, f.secondary].map((cwd, root) => Array.from({ length: 2606 }, (_, i) => ({
    sessionId: `session-${String(i * 2 + root).padStart(5, "0")}`, cwd,
    lastModified: 100000 - (i * 2 + root), summary: `Chat ${i * 2 + root}`,
  })));
  f.bridge.withSdk = action => action({ listSessions: async options => {
    calls.push(options); const source = rows[options.dir === f.primary ? 0 : 1];
    return source.slice(options.offset, options.offset + options.limit);
  } });
  const excluded = rows.flat().map(r => r.sessionId).filter(id => Number(id.slice(-5)) < 5005);
  const params = { dirs: [f.primary, f.secondary], limit: 30, excludeSessionIds: excluded };
  const first = await f.bridge.projectSessions(params);
  assert.equal(first.partial, false);
  assert.equal(first.data[0].sessionId, "session-05005");
  assert.equal(first.data.length, 30);
  assert.ok(first.nextCursor);
  const second = await f.bridge.projectSessions({ ...params, cursor: first.nextCursor });
  assert.deepEqual(await f.bridge.projectSessions({ ...params, cursor: first.nextCursor }), second);
  const all = [...first.data, ...second.data];
  for (let cursor = second.nextCursor; cursor;) {
    const page = await f.bridge.projectSessions({ ...params, cursor });
    assert.equal(page.partial, false); all.push(...page.data); cursor = page.nextCursor;
  }
  assert.deepEqual(all.map(r => r.sessionId), rows.flat().filter(r => !excluded.includes(r.sessionId))
    .sort((a, b) => b.lastModified - a.lastModified).map(r => r.sessionId));
  assert.ok(calls.some(c => c.offset > 2500));
  assert.ok(calls.every(c => c.limit <= 101));
  await assert.rejects(f.bridge.projectSessions({ ...params, dirs: [f.primary], cursor: first.nextCursor }), /changed/);
  // Inserting before a consumed native boundary invalidates the offsets.
  rows[0].unshift({ ...rows[0][0], sessionId: "inserted", lastModified: 200000 });
  await assert.rejects(f.bridge.projectSessions({ ...params, cursor: first.nextCursor }), /changed/);
  assert.equal(f.capturedOptions.length, 0);
  await assert.rejects(f.bridge.projectSessions({ ...params, offset: 5002 }), /cursor/);
});

test("native catalog yields explicit continuation after its bounded scan or a failed root", async t => {
  const f = await fixture(t); let fail = true, calls = 0;
  const rows = Array.from({ length: 6602 }, (_, i) => ({ sessionId: `s-${i}`, cwd: f.primary, lastModified: 10000 - i }));
  f.bridge.withSdk = action => action({ listSessions: async ({ dir, offset, limit }) => {
    calls++; if (dir === f.secondary && fail) throw new Error("Unavailable");
    return dir === f.primary ? rows.slice(offset, offset + limit) : [];
  } });
  const params = { dirs: [f.primary, f.secondary], limit: 2, excludeSessionIds: rows.slice(0, 6600).map(r => r.sessionId) };
  const failed = await f.bridge.projectSessions(params);
  assert.equal(failed.partial, true); assert.deepEqual(failed.data, []); assert.ok(failed.nextCursor);
  fail = false; calls = 0;
  const bounded = await f.bridge.projectSessions({ ...params, cursor: failed.nextCursor });
  assert.equal(bounded.partial, true); assert.deepEqual(bounded.data, []); assert.ok(bounded.nextCursor);
  assert.equal(calls, 65, "64 budgeted reads of sessions plus the empty folder, which settles without budget");
  const done = await f.bridge.projectSessions({ ...params, cursor: bounded.nextCursor });
  assert.equal(done.partial, false); assert.equal(done.nextCursor, null);
  assert.deepEqual(done.data.map(r => r.sessionId), ["s-6600", "s-6601"]);
  // Invalid native pages are incomplete, never an empty complete catalog.
  f.bridge.withSdk = action => action({ listSessions: async () => ({ data: [] }) });
  const malformed = await f.bridge.projectSessions({ dirs: [f.primary] });
  assert.equal(malformed.partial, true); assert.ok(malformed.nextCursor);
  const exact = { dirs: [f.primary], sessionId: "00000000-0000-4000-8000-000000009999" };
  for (const info of [null, {}, { sessionId: exact.sessionId, cwd: f.primary }]) {
    f.bridge.withSdk = action => action({ getSessionInfo: async () => info });
    assert.equal((await f.bridge.projectSessions(exact)).partial, true, "Malformed native lookup cannot prove absence");
  }
  f.bridge.withSdk = action => action({ getSessionInfo: async () => undefined });
  assert.deepEqual((await f.bridge.projectSessions(exact)), { data: [], nextCursor: null, partial: false });
});

test("native catalog deadline includes SDK acquisition and retains uncancellable read slots", async t => {
  const f = await fixture(t); let acquire = 0, release, admitted, leases = 0;
  const blocked = new Promise(resolve => { release = resolve; });
  const fourReads = new Promise(resolve => { admitted = resolve; });
  let allReleased;
  const leasesReleased = new Promise(resolve => { allReleased = resolve; });
  f.bridge.updates.acquire = async () => {
    if (++acquire === 4) admitted();
    await blocked;
    return { runtime: { sdk: { listSessions: async () => [] } }, release: async () => { if (++leases === 4) allReleased(); } };
  };
  const params = { dirs: [f.primary] }, started = performance.now();
  const reads = [f.bridge.projectSessions(params), f.bridge.projectSessions(params),
    ...[1, 2, 3].map(id => f.bridge.projectSessions({ ...params, excludeSessionIds: [`excluded-${id}`] }))];
  // Directory observations can complete out of invocation order. Establish
  // four actual native reads before exercising the admission limit.
  await fourReads;
  const capped = await f.bridge.projectSessions({ ...params, excludeSessionIds: ["fifth"] });
  assert.equal(capped.partial, true);
  assert.equal(acquire, 4, "A fifth distinct request cannot start another uncancellable read");
  const pages = await Promise.all(reads), [a, b] = pages;
  assert.equal(acquire, 4); assert.equal(leases, 0);
  assert.equal(a.partial, true); assert.deepEqual(a, b); assert.ok(a.nextCursor);
  assert.ok(performance.now() - started < 2000);
  assert.equal(f.bridge.sessionFiles.catalogReads.size, 4);
  release();
  // Each read first looks at its folder on disk, which takes more than one tick.
  await leasesReleased;
  assert.equal(leases, 4, "Timed-out SDK leases release when native work actually ends");
  for (const [index, request] of [params, params, ...[1, 2, 3].map(id => ({ ...params, excludeSessionIds: [`excluded-${id}`] }))].entries()) {
    if (index === 1) continue; // The first two callers share one native read.
    assert.equal((await f.bridge.projectSessions({ ...request, cursor: pages[index].nextCursor })).partial, false);
  }
  assert.equal(acquire, 4, "The next polls consume the completed native reads");
  assert.equal(f.bridge.sessionFiles.catalogReads.size, 0);
  assert.equal(f.bridge.sessionFiles.catalogPages.size, 4, "Completed first pages stay bounded too");
  assert.equal((await f.bridge.projectSessions(params)).partial, false);
  assert.equal(acquire, 5, "A fresh refresh starts a new native read");
});

// A slow folder must not restart on every fresh sidebar refresh. Exercise the
// consumer contract here, alongside the native cursor/budget owner above.
test("native catalog fresh first pages adopt slow reads and revalidate completed rows", async t => {
  const f = await fixture(t);
  const folder = join(f.bridge.sessionFiles.root, projectFolderName(f.primary));
  await mkdir(folder, { recursive: true });
  let calls = 0, pending;
  let rows = [{ sessionId: "slow-session", cwd: f.primary, lastModified: 1000, summary: "Worker task" }];
  f.bridge.withSdk = action => (pending = action({ listSessions: async () => {
    calls++;
    const snapshot = [...rows];
    await new Promise(resolve => setTimeout(resolve, 1250));
    return snapshot;
  } }));
  const params = { dirs: [f.primary], limit: 10 };
  const cold = await f.bridge.projectSessions(params);
  assert.equal(cold.partial, true); assert.deepEqual(cold.data, []);
  const adopted = await f.bridge.projectSessions(params); // Still in flight, no cursor.
  assert.equal(adopted.partial, false);
  assert.deepEqual(adopted.data.map(row => row.sessionId), ["slow-session"]);
  assert.equal(calls, 1, "Fresh requests share the same bounded native read");
  const started = performance.now();
  const warm = await f.bridge.projectSessions(params);
  assert.deepEqual(warm, adopted);
  assert.ok(performance.now() - started < 500, "A completed first page is immediately available");
  await pending;
  // Appends need no directory change: the warm read refreshes in the background.
  rows = [{ ...rows[0], lastModified: 2000, summary: "Worker still running" }];
  assert.deepEqual(await f.bridge.projectSessions(params), adopted);
  await pending;
  const refreshed = await f.bridge.projectSessions(params);
  assert.equal(refreshed.data[0].title, "Worker still running");
  await pending;
  // A new transcript invalidates the completed first page before serving it.
  rows = [{ sessionId: "new-worker", cwd: f.primary, lastModified: 3000 }, ...rows];
  await writeFile(join(folder, "new-worker.jsonl"), "{}\n");
  const changed = await f.bridge.projectSessions(params);
  assert.equal(changed.partial, true); assert.deepEqual(changed.data, []);
  await pending;
  const completed = await f.bridge.projectSessions(params);
  assert.equal(completed.partial, false);
  assert.deepEqual(completed.data.map(row => row.sessionId), ["new-worker", "slow-session"]);
  await pending;
  assert.equal(f.capturedOptions.length, 0, "Catalog refresh never starts model work");
});

test("native catalog timeout preserves consumed filtered positions", async t => {
  const f = await fixture(t); let release, calls = 0;
  const blocked = new Promise(resolve => { release = resolve; });
  const rows = Array.from({ length: 102 }, (_, i) => ({ sessionId: `s-${i}`, cwd: f.primary, lastModified: 1000 - i }));
  f.bridge.withSdk = action => action({ listSessions: async ({ offset, limit }) => {
    calls++; if (calls === 2) await blocked;
    return rows.slice(offset, offset + limit);
  } });
  const params = { dirs: [f.primary], limit: 2, excludeSessionIds: rows.slice(0, 100).map(r => r.sessionId) };
  const first = await f.bridge.projectSessions(params);
  assert.equal(first.partial, true); assert.deepEqual(first.data, []);
  assert.equal(JSON.parse(Buffer.from(first.nextCursor, "base64url")).roots[0].offset, 100);
  release(); await new Promise(resolve => setImmediate(resolve));
  const second = await f.bridge.projectSessions({ ...params, cursor: first.nextCursor });
  assert.deepEqual(second.data.map(r => r.sessionId), ["s-100", "s-101"]);
  assert.equal(second.nextCursor, null);
  assert.equal(f.bridge.sessionFiles.catalogReads.size, 0);
});

test("native catalog discovers worktree folders beyond the configured-folder limit", async t => {
  const f = await fixture(t);
  // A repository with 136 linked worktrees (measured: ACSRecomp), most without a
  // Claude transcript. The daemon gives a first page one continuation.
  const dirs = Array.from({ length: 136 }, (_, i) => join(f.primary, `worktree-${String(i).padStart(3, "0")}`));
  const id = "00000000-0000-4000-8000-000000000080";
  const calls = [];
  f.bridge.withSdk = action => action({
    listSessions: async ({ dir }) => {
      calls.push(dir);
      return dir === dirs.at(-1) ? [{ sessionId: id, cwd: dir, firstPrompt: "Worktree task", lastModified: 1000 }] : [];
    },
    getSessionInfo: async (sessionId, { dir }) => dir === dirs.at(-1)
      ? { sessionId, cwd: dir, firstPrompt: "Worktree task", lastModified: 1000 } : undefined,
  });
  const first = await f.bridge.projectSessions({ dirs, limit: 2 });
  assert.equal(first.partial, false, "Empty worktree folders must not starve the first page");
  assert.deepEqual(first.data.map(row => [row.sessionId, row.cwd]), [[id, dirs.at(-1)]]);
  assert.equal(first.nextCursor, null);
  assert.equal(calls.length, dirs.length);
  const exact = await f.bridge.projectSessions({ dirs, sessionId: id });
  assert.deepEqual(exact.data.map(row => row.sessionId), [id]);
  assert.equal(exact.partial, false);

  // Opening a chat asks only the folder whose transcript exists, however many
  // worktrees come first (a real Project with 54 timed out at one second).
  const folder = join(f.bridge.sessionFiles.root, projectFolderName(dirs.at(-1)));
  await mkdir(folder, { recursive: true });
  await writeFile(join(folder, `${id}.jsonl`), "");
  const asked = [], real = { getSessionInfo: async (sessionId, { dir }) => { asked.push(dir); return { sessionId, cwd: dir, lastModified: 1000 }; } };
  f.bridge.withSdk = action => action(real);
  const found = await f.bridge.projectSessions({ dirs, sessionId: id });
  assert.equal(found.partial, false);
  assert.deepEqual(found.data.map(row => row.cwd), [dirs.at(-1)]);
  assert.deepEqual(asked, [dirs.at(-1)]);

  // Folders holding sessions still spend the read budget. Once empty folders
  // are settled, a continuation or a fresh refresh resumes the heads already read.
  const busy = new Set(dirs.slice(36)), read = new Map();
  f.bridge.withSdk = action => action({ listSessions: async ({ dir }) => {
    read.set(dir, (read.get(dir) ?? 0) + 1);
    return busy.has(dir) ? [{ sessionId: `session-${dir.slice(-3)}`, cwd: dir, lastModified: 1000 + Number(dir.slice(-3)) }] : [];
  } });
  for (const [resume, limit] of [["continuation", 3], ["fresh", 4]]) {
    read.clear();
    let page = await f.bridge.projectSessions({ dirs, limit }), polls = 1;
    while (page.partial && page.data.length === 0) {
      assert.ok(++polls <= 2, `${resume}: retained heads must make progress`);
      page = await f.bridge.projectSessions({ dirs, limit, cursor: resume === "fresh" ? null : page.nextCursor });
    }
    assert.equal(polls, 2, `${resume}: 100 folders with sessions take two bounded reads`);
    assert.deepEqual(page.data.map(row => row.sessionId), ["session-135", "session-134", "session-133", "session-132"].slice(0, limit), resume);
    assert.ok([...read.values()].every(count => count === 1), `${resume}: each folder head is read once`);
    assert.equal(read.size, dirs.length, resume);
  }
});

// The phone refreshes every Project's list at once. Eight fast native reads
// must all complete (an empty Project answers "no chats", not "still
// reading"), cold and warm; only the uncancellable-read cap may hold one back.
test("native catalog lists every Project of a concurrent refresh, including an empty one", async t => {
  const root = await mkdtemp(join(tmpdir(), "wonder-native-refresh-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const config = join(root, "config"), files = await nativeFiles(t, config);
  const projects = [];
  for (let p = 0; p < 8; p++) {
    // Project 0 has no Claude chats; project 7 has many linked worktrees.
    const dirs = [];
    for (let w = 0; w < (p === 7 ? 40 : 1); w++) {
      await mkdir(join(root, `p${p}`, `w${w}`), { recursive: true });
      dirs.push(await realpath(join(root, `p${p}`, `w${w}`)));
    }
    for (let i = 0; p > 0 && i < 3; i++) {
      const cwd = dirs[i % dirs.length], sessionId = `00000000-0000-4000-8${p}00-${String(i).padStart(12, "0")}`;
      const folder = join(config, "projects", projectFolderName(cwd)); await mkdir(folder, { recursive: true });
      await writeFile(join(folder, `${sessionId}.jsonl`), JSON.stringify({ type: "user", sessionId, uuid: `user-${p}-${i}`,
        parentUuid: null, cwd, timestamp: new Date().toISOString(), message: { role: "user", content: `Chat ${p}.${i}` } }) + "\n");
    }
    projects.push(dirs);
  }
  for (const round of ["cold", "warm"]) {
    const started = performance.now();
    const pages = await Promise.all(projects.map(dirs => files.listSessions({ dirs, limit: 8 })));
    assert.deepEqual(pages.map(page => page.partial), projects.map(() => false), `${round}: no Project is left "still reading"`);
    assert.deepEqual(pages.map(page => page.data.length), projects.map((_, p) => p ? 3 : 0), round);
    assert.ok(performance.now() - started < 1000, `${round}: answered within the page budget`);
  }
});

test("native file-backed catalog preserves millisecond and ID ordering and opens an older exact UUID", async t => {
  const root = await mkdtemp(join(tmpdir(), "wonder-native-catalog-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const config = join(root, "config");
  let files = await nativeFiles(t, config);
  const dirs = [];
  for (const label of ["app", "docs"]) { await mkdir(join(root, label)); dirs.push(await realpath(join(root, label))); }
  const expected = [];
  for (let i = 0; i < 8; i++) {
    const cwd = dirs[i % 2], sessionId = `00000000-0000-4000-8000-${String(i).padStart(12, "0")}`;
    const folder = join(config, "projects", projectFolderName(cwd)); await mkdir(folder, { recursive: true });
    const path = join(folder, `${sessionId}.jsonl`), at = 1700000000 + Math.floor(i / 2) / 1000;
    await writeFile(path, JSON.stringify({ type: "user", sessionId, uuid: `user-${i}`, parentUuid: null, cwd,
      timestamp: new Date(at * 1000).toISOString(), message: { role: "user", content: `Synthetic chat ${i}` } }) + "\n");
    await utimes(path, at, at); expected.unshift(sessionId);
  }
  const actual = [];
  let cursor = null;
  do {
    const page = await files.listSessions({ dirs, cursor, limit: 3 });
    assert.equal(page.partial, false); actual.push(...page.data.map(r => r.sessionId)); cursor = page.nextCursor;
  } while (cursor);
  assert.deepEqual(actual, expected);
  const exact = await files.listSessions({ dirs, sessionId: expected.at(-1) });
  assert.deepEqual(exact.data.map(r => r.sessionId), [expected.at(-1)]);
  assert.equal(exact.partial, false);

  // Slow native filesystem reads must finish across polls, not restart at the
  // first folder every time. Delay actual SDK results; no model is started.
  const sdk = await import("@anthropic-ai/claude-agent-sdk");
  // A folder that is already indexed needs no SDK read, so a new instance
  // shows the cold reads; the warm instance is checked at the end.
  const warm = files;
  files = await nativeFiles(t, config);
  let pending, calls = 0;
  const slow = method => async (...args) => {
    calls++;
    const result = await sdk[method](...args);
    await new Promise(resolve => setTimeout(resolve, 1200));
    return result;
  };
  files.withSdk = action => (pending = action({ ...sdk,
    listSessions: slow("listSessions"), getSessionInfo: slow("getSessionInfo") }));
  for (const request of [{ dirs, limit: 20 }, { dirs, sessionId: "00000000-0000-4000-8000-000000000001" }]) {
    let cursor = null, recovered;
    for (let attempt = 0; attempt < 4; attempt++) {
      const page = await files.listSessions({ ...request, cursor });
      await pending; await new Promise(resolve => setImmediate(resolve));
      if (!request.sessionId && attempt === 0) {
        const id = "00000000-0000-4000-8000-000000009999";
        const path = join(config, "projects", projectFolderName(dirs[0]), `${id}.jsonl`);
        await writeFile(path, JSON.stringify({ type: "user", sessionId: id, uuid: "new", parentUuid: null,
          cwd: dirs[0], timestamp: new Date().toISOString(), message: { role: "user", content: "New synthetic chat" } }) + "\n");
        const resume = files.withSdk;
        try {
          files.withSdk = action => action(sdk);
          const resized = await files.listSessions({ dirs, cursor: page.nextCursor, limit: 3 });
          assert.equal(resized.data.length, 3, "A changed page size can restart from the native cursor");
          const fresh = await files.listSessions({ dirs, limit: 20 });
          assert.equal(fresh.partial, false);
          assert.ok(fresh.data.some(row => row.sessionId === id), "A fresh refresh cannot consume an older continuation");
        } finally { files.withSdk = resume; await rm(path); }
      }
      if (!page.partial) { recovered = page; break; }
      cursor = page.nextCursor;
    }
    assert.deepEqual(recovered?.data.map(row => row.sessionId), request.sessionId ? [request.sessionId] : expected);
  }
  // The unslowed refresh in the middle indexed the second folder, so only the
  // first folder's slow read completes (once, not restarted) besides the lookup.
  assert.equal(calls, 2, "Each unindexed native folder read completes once; exact lookup asks only the folder holding the transcript");
  // Completed requests are consumed: a new refresh reads native metadata again.
  files.withSdk = action => action(sdk);
  assert.deepEqual((await files.listSessions({ dirs, limit: 20 })).data.map(row => row.sessionId), expected);
  // Folders that are already indexed are not listed again while their files are unchanged.
  warm.withSdk = action => action({ ...sdk, listSessions: async () => { throw new Error("The index should answer"); } });
  assert.deepEqual((await warm.listSessions({ dirs, limit: 20 })).data.map(row => row.sessionId), expected);
});

test("native history includes Claude Code turns and reuses Cupola receipts by identity", () => {
  const turns = nativeTurns([
    { type: "user", uuid: "turn-1", message: { content: "From Wonder" } },
    { type: "assistant", uuid: "a1", message: { id: "msg-1", content: [{ type: "thinking", thinking: "..." }] } },
    { type: "assistant", uuid: "a2", message: { id: "msg-1", content: [{ type: "text", text: "Done" }] } },
    { type: "user", uuid: "cli-1", message: { content: [{ type: "text", text: "From the terminal" }] } },
    { type: "assistant", uuid: "a3", message: { id: "msg-2", content: [{ type: "tool_use", id: "tool-1", name: "Bash", input: { command: "ls" } }] } },
    { type: "user", uuid: "r1", message: { content: [{ type: "tool_result", tool_use_id: "tool-1", content: "ok" }] } },
    // A proposed plan is a plan item under the tool call's ID, like the live stream; a call without text shows nothing.
    { type: "assistant", uuid: "a4", message: { id: "msg-3", content: [{ type: "tool_use", id: "plan-1", name: "ExitPlanMode", input: { plan: "# Plan\n1. Fix it" } },
      { type: "tool_use", id: "plan-2", name: "ExitPlanMode", input: {} }] } },
    { type: "user", uuid: "r2", message: { content: [{ type: "tool_result", tool_use_id: "plan-1", is_error: true, content: "Owner reviews the plan" }] } },
  ], [{ id: "turn-1", status: "completed", items: [{ type: "userMessage", id: "u-1", clientId: "client-1", content: [] }] }]);
  assert.deepEqual(turns.map(t => t.id), ["turn-1", "cli-1"]);
  assert.deepEqual(turns[1].items.slice(2), [{ type: "plan", id: "plan-1", text: "# Plan\n1. Fix it", status: "completed" }]);
  assert.equal(turns[0].items[0].clientId, "client-1");
  // Same ID shape as the live stream: turn:message:content-block-index.
  assert.deepEqual(turns[0].items[1], { type: "reasoning", id: "turn-1:msg-1:thinking:0", status: "completed", summary: ["..."] });
  assert.equal(turns[0].items[2].id, "turn-1:msg-1:1");
  // Shell commands use Codex's command row: the command and its output.
  assert.deepEqual(turns[1].items[1], { type: "commandExecution", id: "tool-1", command: "ls", status: "completed", toolAction: { kind: "command" }, success: true, aggregatedOutput: "ok" });
});

// Claude Code records each thinking block's text in the transcript. History
// shows it as the same thinking row the live stream completes (same ID), in
// order between tool calls and replies, for a session any app holds (Cupola,
// T3 Code, Claude on the Mac or a terminal all write this transcript). Redacted
// and empty thinking add nothing. Redaction and the Thinking detail level are
// the host's (crates/wonderd history.rs).
test("native history keeps thinking text in order between tools and replies", () => {
  const assistant = (uuid, id, block) => ({ type: "assistant", uuid, message: { id, content: [block] } });
  const turns = nativeTurns([
    { type: "user", uuid: "turn-1", message: { content: "Fix the parser" } },
    assistant("a1", "msg-1", { type: "thinking", thinking: "Look at the parser first.\n\nThen run the tests.", signature: "sig" }),
    assistant("a2", "msg-1", { type: "tool_use", id: "tool-1", name: "Bash", input: { command: "ls" } }),
    { type: "user", uuid: "r1", message: { content: [{ type: "tool_result", tool_use_id: "tool-1", content: "ok" }] } },
    assistant("a3", "msg-2", { type: "redacted_thinking", data: "encrypted" }),
    assistant("a4", "msg-2", { type: "thinking", thinking: "", signature: "sig" }),
    assistant("a5", "msg-2", { type: "thinking", thinking: "The fix is one line." }),
    assistant("a6", "msg-2", { type: "text", text: "Fixed." }),
  ], [], true);
  assert.deepEqual(turns[0].items.slice(1).map(i => [i.type, i.id, i.summary]), [
    ["reasoning", "turn-1:msg-1:thinking:0", ["Look at the parser first.", "Then run the tests."]],
    ["commandExecution", "tool-1", undefined],
    ["reasoning", "turn-1:msg-2:thinking:2", ["The fix is one line."]],
    ["agentMessage", "turn-1:msg-2:3", undefined],
  ]);
  assert.ok(turns[0].items.every(i => i.type !== "reasoning" || i.status === "completed"));
  assert.ok(!JSON.stringify(turns).includes("encrypted"));
  // Thinking is not work: a reply before more thinking stays a reply, as live.
  const [reply] = nativeTurns([
    { type: "user", uuid: "turn-2", message: { content: "Hi" } },
    assistant("b1", "msg-3", { type: "text", text: "Hello." }),
    assistant("b2", "msg-4", { type: "thinking", thinking: "Anything else?" }),
    assistant("b3", "msg-4", { type: "text", text: "Ask away." }),
  ]);
  assert.deepEqual(reply.items.slice(1).map(i => [i.type, i.phase]), [["agentMessage", undefined], ["reasoning", undefined], ["agentMessage", undefined]]);
});

test("images the owner attaches in Claude Code reach the prompt, bounded", () => {
  // Shapes recorded by Claude Code and the Claude desktop app: base64 image
  // blocks beside or instead of the typed words (paste ids shortened).
  const png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
  const image = (media_type, data = png) => ({ type: "image", source: { type: "base64", media_type, data } });
  const turns = nativeTurns([
    { type: "user", uuid: "u1", imagePasteIds: [1], origin: { kind: "human" },
      message: { role: "user", content: [image("image/webp"), { type: "text", text: "What is in [Image #1]?" }] } },
    { type: "assistant", uuid: "a1", message: { id: "m1", content: [{ type: "text", text: "A cat." }] } },
    // An image alone is still a prompt; the reply belongs to it, not the turn before.
    { type: "user", uuid: "u2", imagePasteIds: [1, 2], message: { role: "user", content: [image("image/png"), image("image/png", "A".repeat(11_184_816))] } },
    { type: "assistant", uuid: "a2", message: { id: "m2", content: [{ type: "tool_use", id: "t1", name: "Read", input: { file_path: "/p/shot.png" } }] } },
    // A tool's image result is the tool's output, not something the owner attached.
    { type: "user", uuid: "r1", message: { content: [{ type: "tool_result", tool_use_id: "t1", content: [image("image/png")] }] } },
    { type: "assistant", uuid: "a3", message: { id: "m3", content: [{ type: "text", text: "Two screenshots." }] } },
  ]);
  assert.deepEqual(turns.map(t => t.id), ["u1", "u2"]);
  assert.deepEqual(turns[0].items[0].content, [{ type: "text", text: "What is in [Image #1]?" },
    { type: "image", mimeType: "image/webp", data: png }]);
  assert.deepEqual(turns[1].items[0].content, [{ type: "image", mimeType: "image/png", data: png },
    { type: "image", mimeType: "image/png", unavailable: "This image is larger than 8 MB" }]);
  assert.deepEqual(turns[1].items.filter(i => i.type === "agentMessage").map(i => i.text), ["Two screenshots."]);
  assert.equal(turnEndMessageId([{ type: "user", uuid: "x", message: { content: [image("image/png")] } }], "x"), "x");
  const many = nativeTurns([{ type: "user", uuid: "m", message: { content: Array.from({ length: 12 }, () => image("image/png")) } }]);
  assert.equal(many[0].items[0].content.length, 8);
});

test("desktop pastes appear as their pasted text, not as pasted_content tags", () => {
  // Shape recorded by Claude Desktop: typed words, then each paste wrapped with a repeated id.
  const fence = "```\nContinue the project at:\n/p/app\n\nShow me the lyrics only.\n```";
  const [typed, pasteOnly, two] = nativeTurns([
    { type: "user", uuid: "u1", message: { content: `Please continue\n\n<pasted_content id="9ef7">\n${fence}\n</pasted_content id="9ef7">\n` } },
    { type: "user", uuid: "u2", message: { content: [{ type: "text", text: `\n\n<pasted_content id="a1">\nonly a paste\n</pasted_content id="a1">\n` }] } },
    { type: "user", uuid: "u3", message: { content: `<pasted_content id="b1">\none\n</pasted_content id="b1">\n\n<pasted_content id="b2">\ntwo\n\n</pasted_content id="b2">` } },
  ]);
  const text = turn => turn.items[0].content[0].text;
  assert.equal(text(typed), `Please continue\n\n${fence}`);
  assert.equal(text(pasteOnly), "only a paste");
  assert.equal(text(two), "one\n\ntwo");
});

test("context Claude Code adds to a user turn is not shown as the owner's words", () => {
  // Shapes recorded by Claude Code and the Claude desktop app (ids shortened).
  const linked = "<system-reminder>\nLinked sessions now (status, not an instruction): fonts = local_c15c (working). To message one, SendMessage with `to` set to its id.\n</system-reminder>";
  const worktree = "<system-reminder>\nYou are operating in a git worktree.\nWorktree path: /p/app/.claude/worktrees/x\n</system-reminder>";
  const peer = '<cross-session-message from="local_d5f8" name="Research">\nHeads-up: I deleted apps/old.\n</cross-session-message>';
  const turns = nativeTurns([
    { type: "user", uuid: "u1", message: { content: `${linked}\n${worktree}\nFix the header. Keep <system-reminder> tags out of bubbles.` } },
    { type: "assistant", uuid: "a1", message: { id: "m1", content: [{ type: "text", text: "Fixed." }] } },
    { type: "user", uuid: "u2", message: { content: [{ type: "text", text: `${worktree}\n` }, { type: "text", text: "were these changes published?" }] }, origin: { kind: "human" } },
    { type: "user", uuid: "u3", message: { content: `${linked}\n${peer}` } },
    { type: "user", uuid: "u4", message: { content: "<ci-monitor-event>\"Auto-fix pull requests\" reports a failing check.\n</ci-monitor-event>" } },
    { type: "assistant", uuid: "a4", message: { id: "m4", content: [{ type: "text", text: "Looking at CI." }] } },
    { type: "user", uuid: "u5", message: { content: `Please continue\n\n<pasted_content id="p1">\n${worktree}\n</pasted_content id="p1">` } },
  ]);
  const prompt = turn => turn.items[0].content[0].text;
  // Injected blocks go; a tag the owner typed inline stays. Context-only entries open no turn.
  assert.deepEqual(turns.map(t => t.id), ["u1", "u2", "u5"]);
  assert.equal(prompt(turns[0]), "Fix the header. Keep <system-reminder> tags out of bubbles.");
  assert.equal(prompt(turns[1]), "were these changes published?");
  assert.deepEqual(turns[1].items.slice(1).map(i => i.text), ["Looking at CI."]);
  // Text the owner pasted is theirs, even when it looks like a reminder.
  assert.equal(prompt(turns[2]), `Please continue\n\n${worktree}`);
  assert.equal(turnEndMessageId([{ type: "user", uuid: "u3", message: { content: linked } }], "u3"), null);
  assert.equal(sessionSummary({ sessionId: "s", firstPrompt: `${worktree}\nRename the app`, lastModified: 1000 }).title, "Rename the app");
});

test("a shell or slash command and its output form one turn whose prompt keeps the command markup", () => {
  // Shapes recorded by Claude Code: `!` runs, local and prompt slash commands (meta entries are filtered upstream).
  const messages = [
    { type: "user", uuid: "c1", message: { content: "<bash-input>git status --short</bash-input>" } },
    { type: "user", uuid: "c1o", message: { content: "<bash-stdout> M README.md</bash-stdout><bash-stderr></bash-stderr>" } },
    { type: "user", uuid: "c2", message: { content: "<command-name>/model</command-name>\n            <command-message>model</command-message>\n            <command-args></command-args>" } },
    { type: "user", uuid: "c2o", message: { content: "<local-command-stdout>Set model to \u001b[1mOpus\u001b[22m</local-command-stdout>" } },
    { type: "user", uuid: "c3", message: { content: "<command-message>review is running…</command-message>\n<command-name>/review</command-name>\n<command-args>42</command-args>" } },
    { type: "assistant", uuid: "a3", message: { id: "m3", content: [{ type: "text", text: "Reviewed." }] } },
  ];
  const turns = nativeTurns(messages);
  assert.deepEqual(turns.map(t => t.id), ["c1", "c2", "c3"]);
  assert.equal(turns[0].items[0].content[0].text, "<bash-input>git status --short</bash-input>\n<bash-stdout> M README.md</bash-stdout><bash-stderr></bash-stderr>");
  assert.match(turns[1].items[0].content[0].text, /<command-name>\/model<\/command-name>[\s\S]*\n<local-command-stdout>Set model to/);
  assert.equal(turns[2].items.at(-1).text, "Reviewed.");
  // Forking at the shell command keeps its output.
  assert.equal(turnEndMessageId(messages, "c1"), "c1o");
});

test("text between Claude tool calls is commentary; text after the last one is the reply", () => {
  const [turn] = nativeTurns([
    { type: "user", uuid: "turn-1", message: { content: "Fix it" } },
    { type: "assistant", uuid: "a1", message: { id: "m1", content: [{ type: "text", text: "Looking" },
      { type: "tool_use", id: "t1", name: "Bash", input: { command: "ls" } }] } },
    { type: "assistant", uuid: "a2", message: { id: "m2", content: [{ type: "text", text: "Now editing" },
      { type: "tool_use", id: "t2", name: "Read", input: { file_path: "/p/a" } }] } },
    { type: "assistant", uuid: "a3", message: { id: "m3", content: [{ type: "text", text: "Done" }] } },
  ]);
  const messages = turn.items.filter(item => item.type === "agentMessage");
  assert.deepEqual(messages.map(item => [item.text, item.phase]), [["Looking", "commentary"], ["Now editing", "commentary"], ["Done", undefined]]);
});

test("Claude file tools become file changes with patches, including failures", () => {
  const turns = nativeTurns([
    { type: "user", uuid: "turn-1", message: { content: "Edit" } },
    { type: "assistant", uuid: "a1", message: { id: "m1", content: [
      { type: "tool_use", id: "w", name: "Write", input: { file_path: "/p/new.txt", content: "one\ntwo\n" } },
      { type: "tool_use", id: "e", name: "Edit", input: { file_path: "/p/a.swift", old_string: "let a = 1", new_string: "let a = 2\nlet b = 3" } },
      { type: "tool_use", id: "m", name: "MultiEdit", input: { file_path: "/p/b.swift", edits: [{ old_string: "x", new_string: "y" }, { old_string: "z", new_string: "" }] } },
      { type: "tool_use", id: "r", name: "Read", input: { file_path: "/p/a.swift" } }] } },
    { type: "user", uuid: "r1", message: { content: [{ type: "tool_result", tool_use_id: "e", is_error: true, content: "String not found" }] } },
  ]);
  const [write, edit, multi, read] = turns[0].items.slice(1);
  assert.deepEqual(write.changes, [{ path: "/p/new.txt", kind: { type: "add" }, diff: "one\ntwo\n" }]);
  assert.equal(edit.changes[0].diff, "@@ @@\n-let a = 1\n+let a = 2\n+let b = 3");
  assert.equal(edit.status, "failed");
  assert.equal(edit.error.message, "String not found");
  assert.equal(multi.changes[0].diff, "@@ @@\n-x\n+y\n@@ @@\n-z");
  assert.equal(read.type, "mcpToolCall");
  assert.deepEqual(read.toolAction, { kind: "read", paths: ["/p/a.swift"] });
  assert.equal(read.status, "unknown", "Without a result, historical tool input is not completed work");
});

// Shapes copied from a real Claude Code transcript: Edit records the hunk it
// applied (3 context lines, 14 added, 3 context), Write of a new file records
// "create". The pill must read +14 -0 and +2 -0, not the tool input's counts.
const historyHunk = { oldStart: 19, oldLines: 6, newStart: 19, newLines: 20, lines: [
  "             .fetch_optional(&self.pool).await", "     }", " ",
  ...Array.from({ length: 14 }, (_, i) => `+    line ${i}`), "     pub async fn next() {", "         todo!()", "     }"] };
const fileEditRows = () => [
  { type: "user", uuid: "turn-1", message: { content: "Edit" } },
  { type: "assistant", uuid: "a1", message: { id: "m1", content: [
    { type: "tool_use", id: "e", name: "Edit", input: { file_path: "/p/history.rs", old_string: "a\nb\nc\nd\ne\nf", new_string: "x".repeat(20).split("").join("\n") } },
    { type: "tool_use", id: "w", name: "Write", input: { file_path: "/p/new.rs", content: "one\ntwo\n" } },
    { type: "tool_use", id: "o", name: "Write", input: { file_path: "/p/old.rs", content: "kept\nnew\n" } },
    { type: "tool_use", id: "x", name: "Edit", input: { file_path: "/p/other.rs", old_string: "a", new_string: "b" } }] } },
  { type: "user", uuid: "r-e", message: { content: [{ type: "tool_result", tool_use_id: "e", content: "ok" }] },
    toolUseResult: { filePath: "/p/history.rs", oldString: "…", newString: "…", originalFile: null, structuredPatch: [historyHunk], userModified: false, replaceAll: false } },
  { type: "user", uuid: "r-w", message: { content: [{ type: "tool_result", tool_use_id: "w", content: "ok" }] },
    toolUseResult: { type: "create", filePath: "/p/new.rs", content: "one\ntwo\n", structuredPatch: [], originalFile: null } },
  { type: "user", uuid: "r-o", message: { content: [{ type: "tool_result", tool_use_id: "o", content: "ok" }] },
    toolUseResult: { type: "update", filePath: "/p/old.rs", content: "kept\nnew\n", originalFile: "kept\nold\n",
      structuredPatch: [{ oldStart: 1, oldLines: 2, newStart: 1, newLines: 2, lines: [" kept", "-old", "+new"] }] } },
  // A result naming another file is not this edit's record.
  { type: "user", uuid: "r-x", message: { content: [{ type: "tool_result", tool_use_id: "x", content: "ok" }] },
    toolUseResult: { filePath: "/p/elsewhere.rs", structuredPatch: [{ oldStart: 1, oldLines: 0, newStart: 1, newLines: 9, lines: ["+z"] }] } },
];
const counts = diff => ["+", "-"].map(m => diff.split("\n").filter(l => l.startsWith(m)).length);

test("Claude file edits count the hunks Claude Code applied, from the transcript and live", async t => {
  // History: the SDK reader drops toolUseResult, so the session file restores it by uuid.
  const root = await mkdtemp(join(tmpdir(), "claude-edits-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const session = "11111111-2222-4333-8444-555555555555";
  await mkdir(join(root, projectFolderName("/p")));
  await writeFile(join(root, projectFolderName("/p"), `${session}.jsonl`), fileEditRows().map(r => JSON.stringify(r)).join("\n") + "\n");
  const edits = await new ClaudeSessionFiles(root).fileEdits(session, "/p");
  assert.deepEqual([...edits.keys()], ["r-e", "r-w", "r-o", "r-x"]);
  const fromSdk = fileEditRows().map(({ toolUseResult, ...row }) => edits.has(row.uuid) ? { ...row, tool_use_result: edits.get(row.uuid) } : row);
  const [edit, write, overwrite, other] = nativeTurns(fromSdk)[0].items.slice(1);
  assert.deepEqual(counts(edit.changes[0].diff), [14, 0]);
  assert.ok(edit.changes[0].diff.startsWith("@@ -19,6 +19,20 @@\n             .fetch_optional"));
  assert.deepEqual(write.changes[0], { path: "/p/new.rs", kind: { type: "add" }, diff: "one\ntwo\n" });
  assert.deepEqual([overwrite.changes[0].kind, counts(overwrite.changes[0].diff)], [{ type: "update" }, [1, 1]]);
  assert.equal(other.changes[0].diff, "@@ @@\n-a\n+b");

  // Live: the SDK hands the same record to the turn as tool_use_result.
  const events = [];
  const live = new TurnProjection({ threadId: "t", turnId: "turn", emit: e => events.push(e) });
  live.start();
  const [, assistant, editResult] = fileEditRows();
  live.accept({ ...assistant, type: "assistant" });
  live.accept({ type: "user", message: editResult.message, tool_use_result: editResult.toolUseResult });
  const done = events.find(e => e.method === "item/completed" && e.params.item.id === "e").params.item;
  assert.deepEqual(counts(done.changes[0].diff), [14, 0]);
});

test("project search is allowed only where no protected folder can be traversed", async t => {
  const f = await fixture(t);
  const denied = join(f.primary, "secrets");
  await mkdir(denied);
  const policy = new ToolPolicy({ ...f.policy, cwd: f.primary, deniedRoots: [denied] });
  assert.equal(await policy.decision("Grep", { path: f.secondary }), "allow");
  assert.equal(await policy.decision("Glob", { path: f.primary }), "deny");
});

test("folder suggestions exclude programmatic sessions and keep the latest activity", async t => {
  const f = await fixture(t);
  let options;
  f.bridge.withSdk = action => action({ listSessions: async o => { options = o; return [
    { sessionId: "a", cwd: "/work/app", lastModified: 5000 }, { sessionId: "b", cwd: "/work/app", lastModified: 9000 },
    { sessionId: "c", cwd: "/work/site", lastModified: 7000 }, { sessionId: "d", cwd: "relative", lastModified: 9999 }]; } });
  const result = await f.bridge.request("project/folders/list", { limit: 5 });
  assert.equal(options.includeProgrammatic, false);
  assert.deepEqual(result.data, [{ cwd: "/work/app", updatedAt: 9 }, { cwd: "/work/site", updatedAt: 7 }]);
});

// Project inputs can read only their own Cupola media inside protected data.
// This exercises the same policy used by image conversion and native Read.
test("project attachments keep other conversations protected", async t => {
  const f = await fixture(t);
  const data = join(f.root, "private"), media = join(data, "project-media", "owned"), other = join(data, "project-media", "other");
  await mkdir(media, { recursive: true }); await mkdir(other, { recursive: true });
  const file = join(media, "attachment"), secret = join(other, "secret");
  await writeFile(file, "attachment"); await writeFile(secret, "secret");
  const policy = new ToolPolicy({ ...f.policy, cwd: f.primary, workspace: media, deniedRoots: [data] });
  assert.equal(await policy.permits(file), true);
  assert.equal(await policy.permits(file, true), false);
  assert.equal(await policy.permits(secret), false);
  assert.equal(await policy.decision("Grep", { path: media }), "deny");
  await symlink(secret, join(media, "escape"));
  assert.equal(await policy.permits(join(media, "escape")), false);
});

const notice = (toolUseId, status, summary) => ({ type: "user", uuid: `n-${toolUseId}`, isQueuedCommand: true,
  origin: { kind: "task-notification", producer: "session-task" },
  message: { content: `<task-notification>\n<task-id>b1</task-id>\n<tool-use-id>${toolUseId}</tool-use-id>\n<output-file>/tmp/x</output-file>\n<status>${status}</status>\n<summary>${summary}</summary>\n</task-notification>` } });

// Background-task notices and the interrupt marker are runtime events, not
// owner messages; tasks come from the tool calls and their notices.
test("native history hides runtime notices and records background tasks", () => {
  const messages = [
    { type: "user", uuid: "turn-1", origin: { kind: "human" }, message: { content: "Build and test" } },
    { type: "assistant", uuid: "a1", message: { id: "m1", content: [
      { type: "tool_use", id: "bg-1", name: "Bash", input: { command: "make", description: "Build the app", run_in_background: true } },
      { type: "tool_use", id: "bg-2", name: "Bash", input: { command: "make test", description: "Run tests", run_in_background: true } },
      { type: "tool_use", id: "agent-1", name: "Agent", input: { description: "Review code", subagent_type: "Explore", prompt: "Look" } },
      { type: "tool_use", id: "fg", name: "Bash", input: { command: "ls" } }] } },
    { type: "user", uuid: "r1", message: { content: [{ type: "tool_result", tool_use_id: "bg-1", content: "Command running in background" },
      { type: "tool_result", tool_use_id: "agent-1", content: [{ type: "text", text: "Looks fine" }] }] } },
    notice("bg-1", "completed", "Background command \"Build the app\" completed (exit code 0)"),
    { type: "user", uuid: "stop", message: { content: [{ type: "text", text: "[Request interrupted by user]" }] } },
    { type: "user", uuid: "turn-2", origin: { kind: "human" }, message: { content: "Continue" } },
  ];
  const turns = nativeTurns(messages, []);
  // The report landed mid-turn, so it opens no run of its own.
  assert.deepEqual(turns.map(t => t.id), ["turn-1", "turn-2"]);
  assert.equal(turns[0].status, "interrupted");
  assert.ok(!JSON.stringify(turns).includes("task-notification"));
  const live = backgroundTasks(messages, true);
  assert.deepEqual(live.map(t => [t.id, t.kind, t.status]), [["agent-1", "agent", "completed"], ["bg-2", "command", "running"], ["bg-1", "command", "completed"]]);
  assert.equal(live[2].summary, "Background command \"Build the app\" completed (exit code 0)");
  assert.equal(live[0].role, "Explore");
  // Once the session stops working on the Mac, a task without a notice is unknown.
  assert.equal(backgroundTasks(messages, false).find(t => t.id === "bg-2").status, "unknown");
});

// Owner (Oct 22, ACSRecomp in T3 Code): two zone-retry loops Claude ended
// with TaskStop stayed "Running on your computer". A stop writes only its
// own tool result ("Successfully stopped task: <id> (...)"); no notice follows.
test("background command outcomes settle the task list in any order", () => {
  const launch = (id, task, at = "2026-10-22T16:29:00Z") => [
    { type: "assistant", uuid: `a-${id}`, timestamp: at, message: { id: `m-${id}`, content: [
      { type: "tool_use", id, name: "Bash", input: { command: "gcloud compute instances create x", description: `Run ${id}`, run_in_background: true } }] } },
    { type: "user", uuid: `r-${id}`, timestamp: at, message: { content: [{ type: "tool_result", tool_use_id: id,
      content: `Command running in background with ID: ${task}. Output is being written to: /tmp/${task}.output` }] } }];
  const say = (id, task, status) => ({ type: "user", uuid: `n-${id}-${status}`, timestamp: "2026-10-22T17:00:00Z", origin: { kind: "task-notification" },
    message: { content: `<task-notification>\n<task-id>${task}</task-id>\n<tool-use-id>${id}</tool-use-id>\n<status>${status}</status>\n<summary>Done</summary>\n</task-notification>` } });
  const call = (id, name, input, content, isError = false) => [
    { type: "assistant", uuid: `a-${id}`, timestamp: "2026-10-22T17:00:34Z", message: { id: `m-${id}`, content: [{ type: "tool_use", id, name, input }] } },
    { type: "user", uuid: `r-${id}`, timestamp: "2026-10-22T17:00:34Z", message: { content: [{ type: "tool_result", tool_use_id: id, content, ...(isError ? { is_error: true } : {}) }] } }];
  const stop = (task, n = "s") => call(`tu-${n}`, "TaskStop", { task_id: task }, `{"message":"Successfully stopped task: ${task} (gcloud ...)","task_id":"${task}"}`);
  const cases = [
    ["start then completed", [...launch("tu-1", "b1"), say("tu-1", "b1", "completed")], { "tu-1": "completed" }],
    ["start then failed", [...launch("tu-1", "b1"), say("tu-1", "b1", "failed")], { "tu-1": "failed" }],
    ["start then killed notice", [...launch("tu-1", "b1"), say("tu-1", "b1", "killed")], { "tu-1": "interrupted" }],
    ["start then TaskStop without a notice", [...launch("tu-1", "b1"), ...stop("b1")], { "tu-1": "interrupted" }],
    ["start then KillShell", [...launch("tu-1", "b1"), ...call("tu-k", "KillShell", { shell_id: "b1" }, "Successfully killed shell: b1")], { "tu-1": "interrupted" }],
    ["a refused stop leaves it running", [...launch("tu-1", "b1"), ...call("tu-s", "TaskStop", { task_id: "b1" }, "No task found", true)], { "tu-1": "running" }],
    ["a read that saw it finish", [...launch("tu-1", "b1"), ...call("tu-o", "BashOutput", { bash_id: "b1" }, "<status>completed</status>\n<exit_code>0</exit_code>")], { "tu-1": "completed" }],
    ["a read while it runs", [...launch("tu-1", "b1"), ...call("tu-o", "BashOutput", { bash_id: "b1" }, "<status>running</status>")], { "tu-1": "running" }],
    ["completed notice then a late stop", [...launch("tu-1", "b1"), say("tu-1", "b1", "completed"), ...stop("b1")], { "tu-1": "completed" }],
    ["notice before the launch result", [launch("tu-1", "b1")[0], say("tu-1", "b1", "completed"), launch("tu-1", "b1")[1]], { "tu-1": "completed" }],
    ["no notice, still running", launch("tu-1", "b1"), { "tu-1": "running" }],
    ["several tasks, one stopped", [...launch("tu-1", "b1"), ...launch("tu-2", "b2"), ...launch("tu-3", "b3"), say("tu-1", "b1", "completed"), ...stop("b2")],
      { "tu-1": "completed", "tu-2": "interrupted", "tu-3": "running" }],
  ];
  for (const [name, messages, expected] of cases) {
    assert.deepEqual(Object.fromEntries(backgroundTasks(messages, true).map(t => [t.id, t.status])), expected, name);
    // A session no longer working on the Mac runs nothing: an unsettled task is unknown, never running.
    const ended = Object.fromEntries(Object.entries(expected).map(([id, status]) => [id, status === "running" ? "unknown" : status]));
    assert.deepEqual(Object.fromEntries(backgroundTasks(messages, false).map(t => [t.id, t.status])), ended, `${name}, session ended`);
  }
});

test("desktop session liveness reads only Claude Code's busy session records", async t => {
  const dir = await mkdtemp(join(tmpdir(), "wonder-claude-sessions-"));
  t.after(() => rm(dir, { recursive: true, force: true }));
  const record = (pid, sessionId, status) => writeFile(join(dir, `${pid}.json`), JSON.stringify({ pid, procStart: pid === process.pid ? processLabel(pid) : null, sessionId, status }));
  await record(process.pid, "live", "busy");
  await writeFile(join(dir, `${process.pid}.abc.key`), "{}");
  assert.equal(await claudeSessionBusy("live", dir), true);
  await record(process.pid, "live", "idle");
  assert.equal(await claudeSessionBusy("live", dir), false);
  await rm(join(dir, `${process.pid}.json`));
  await record(2147483646, "dead", "busy");
  assert.equal(await claudeSessionBusy("dead", dir), false, "A record from an exited process is not running");
  assert.equal(await claudeSessionBusy("missing", join(dir, "absent")), false);
});

// One damaged, oversized, misnamed or recycled-PID record leaves only the
// session it names unknown; a record naming no session holds none. Every
// other chat's state stays readable (it used to fail the whole scan).
test("one bad Claude session record holds only the chat it names", async t => {
  const dir = await mkdtemp(join(tmpdir(), "wonder-claude-sessions-"));
  t.after(() => rm(dir, { recursive: true, force: true }));
  const bad = process.ppid, good = process.pid;
  const named = "aaaaaaaa-0000-4000-8000-000000000001", other = "bbbbbbbb-0000-4000-8000-000000000002";
  await writeFile(join(dir, `${good}.json`), JSON.stringify({ pid: good, procStart: processLabel(good), sessionId: other, status: "busy" }));
  for (const [label, text, holdsNamed] of [
    ["truncated", `{"pid":${bad},"sessionId":"${named}","stat`, true],
    ["oversized", JSON.stringify({ pid: bad, sessionId: named, padding: "x".repeat(64 * 1024) }), true],
    ["another PID", JSON.stringify({ pid: bad + 1, procStart: processLabel(bad), sessionId: named, status: "idle" }), true],
    ["recycled PID", JSON.stringify({ pid: bad, procStart: "another process", sessionId: named, status: "idle" }), true],
    ["names no session", "{", false],
    ["oversized, names no session", "x".repeat(64 * 1024 + 1), false],
  ]) {
    await writeFile(join(dir, `${bad}.json`), text);
    const state = await claudeSessionState(named, dir);
    assert.equal(state.open, holdsNamed ? null : false, `${label}: the chat it names`);
    assert.deepEqual(await claudeSessionState(other, dir), { open: true, busy: true, busySince: null, background: true, startedAt: null }, `${label}: another open chat`);
    assert.equal((await claudeSessionState("cccccccc-0000-4000-8000-000000000003", dir)).open, false, `${label}: a chat open nowhere`);
  }
});

test("a desktop-busy Claude session shows its turn running elsewhere and refuses a second writer", async t => {
  const f = await fixture(t, { transcripts: {} });
  const dir = await mkdtemp(join(tmpdir(), "wonder-claude-sessions-"));
  t.after(() => rm(dir, { recursive: true, force: true }));
  const sdkSessionId = "11111111-2222-3333-4444-555555555555";
  const messages = [{ type: "user", uuid: "turn-1", origin: { kind: "human" }, message: { content: "Working on the Mac" } },
    { type: "assistant", uuid: "a1", message: { id: "m1", content: [{ type: "text", text: "Starting" }] } }];
  const sdk = { getSessionMessages: async () => messages, getSessionInfo: async (id, { dir: cwd }) => ({ sessionId: id, cwd, lastModified: 1 }) };
  const claudeProjectsDir = join(f.root, "claude-projects");
  const folder = join(claudeProjectsDir, projectFolderName(f.primary));
  await mkdir(folder, { recursive: true });
  const transcript = join(folder, `${sdkSessionId}.jsonl`);
  await writeFile(transcript, messages.map(m => JSON.stringify(m)).join("\n") + "\n");
  const bridge = new ClaudeBridge({ updates: { acquire: async () => ({ runtime: { sdk }, release: async () => {} }) },
    sessions: f.sessions, send: async () => {}, claudeSessionsDir: dir, claudeProjectsDir });
  const { thread } = await bridge.request("project/session/attach", { sessionId: sdkSessionId, model: "claude:haiku", wonderPolicy: f.policy, wonderProject: f.project });
  await writeFile(join(dir, `${process.pid}.json`), JSON.stringify({ pid: process.pid, procStart: processLabel(process.pid), sessionId: sdkSessionId, status: "busy" }));
  const { data } = await bridge.request("thread/turns/list", { threadId: thread.id, itemsView: "notLoaded" });
  assert.equal(data[0].status, "inProgress");
  assert.equal(data[0].runningElsewhere, true);
  await assert.rejects(bridge.request("turn/start", { threadId: thread.id, input: [{ type: "text", text: "Another" }] }), /working on this conversation on your computer/);
  await writeFile(join(dir, `${process.pid}.json`), JSON.stringify({ pid: process.pid, procStart: processLabel(process.pid), sessionId: sdkSessionId, status: "idle" }));
  const idle = await bridge.request("thread/turns/list", { threadId: thread.id, itemsView: "notLoaded" });
  assert.equal(idle.data[0].status, "completed");
  assert.equal(idle.data[0].runningElsewhere, undefined);
  // A finished turn that grows (Claude's reply when a background task ends) changes its count.
  assert.deepEqual([idle.data[0].items, idle.data[0].itemCount], [[], 2]);
  const first = await bridge.request("thread/items/list", { threadId: thread.id, limit: 1 });
  const second = await bridge.request("thread/items/list", { threadId: thread.id, limit: 1, cursor: first.nextCursor });
  assert.equal(second.data[0].item.text, "Starting");
  messages.push({ type: "user", origin: { kind: "task-notification" }, message: { content: "<task-notification></task-notification>" } },
    { type: "assistant", uuid: "a2", message: { id: "m2", content: [{ type: "text", text: "The task finished" }] } });
  await appendFile(transcript, messages.slice(-2).map(m => JSON.stringify(m)).join("\n") + "\n");
  const grown = await bridge.request("thread/turns/list", { threadId: thread.id, itemsView: "notLoaded" });
  assert.deepEqual([grown.data[0].id, grown.data[0].itemCount], ["turn-1", 3]);
  await assert.rejects(bridge.request("thread/items/list", { threadId: thread.id, cursor: first.nextCursor }), /history changed/);
  // A durable Cupola outcome or prompt can change while the native file stays still.
  const receipt = { id: "turn-1", status: "interrupted", error: { message: "Stopped" },
    items: [{ type: "userMessage", id: "receipt", content: [{ type: "text", text: "Owned prompt" }] }] };
  f.sessions.get(thread.id).turns.push(receipt);
  const stopped = await bridge.request("thread/turns/list", { threadId: thread.id });
  assert.deepEqual([stopped.data[0].status, stopped.data[0].error, stopped.data[0].items[0]],
    ["interrupted", receipt.error, receipt.items[0]]);
  receipt.items[0].content[0].text = "Updated prompt";
  const updated = await bridge.request("thread/items/list", { threadId: thread.id, limit: 1 });
  assert.equal(updated.data[0].item.content[0].text, "Updated prompt");
  // Losing file evidence must fall back to a fresh SDK read, never cached output.
  await rm(transcript);
  messages.at(-1).message.content[0].text = "Changed without a readable file";
  const missing = await bridge.request("thread/items/list", { threadId: thread.id });
  assert.equal(missing.data.at(-1).item.text, "Changed without a readable file");
});

// A transcript charged above the whole cache budget (about 16 MB of JSONL) is
// still reused for the pages of one history read.
test("a transcript larger than the cache budget is reused across pages", async t => {
  const f = await fixture(t);
  const id = "11111111-2222-4333-8444-555555555557";
  const cwd = await realpath(f.primary);
  const config = join(f.root, "native");
  const files = await nativeFiles(t, config);
  const sdk = await import("@anthropic-ai/claude-agent-sdk");
  const folder = join(config, "projects", projectFolderName(cwd));
  await mkdir(folder, { recursive: true });
  const common = { sessionId: id, cwd, isSidechain: false, timestamp: "2026-10-13T10:00:00Z" };
  await writeFile(join(folder, `${id}.jsonl`), [
    { type: "user", uuid: "u1", parentUuid: null, message: { role: "user", content: "First" } },
    { type: "assistant", uuid: "a1", parentUuid: "u1", message: { role: "assistant", id: "m1", stop_reason: "end_turn", content: [{ type: "text", text: "Done" }] } },
  ].map(row => JSON.stringify({ ...common, ...row })).join("\n") + "\n");
  let reads = 0;
  const reader = { getSessionMessages: async (...args) => { reads++; return sdk.getSessionMessages(...args); } };
  const bridge = new ClaudeBridge({ updates: { acquire: async () => ({ runtime: { sdk: reader }, release: async () => {} }) },
    sessions: f.sessions, send: async () => {}, claudeSessionsDir: join(config, "sessions") });
  bridge.sessionFiles = files;
  bridge.transcripts.maxBytes = 1;
  const session = await f.sessions.create({ wonderProject: { ...f.project, cwd } }, null, { sdkSessionId: id, sdkStarted: true });
  const history = () => bridge.request("thread/turns/list", { threadId: session.id, includeHistoryRevision: true });
  const first = await history(), second = await history();
  assert.equal(reads, 1, "an oversized transcript is not re-read for every page");
  assert.equal(first.historyRevision, second.historyRevision);
  assert.equal(bridge.transcripts.size, 1, "held alone; its expiry is owned by the ByteCache test");
});

// Contract owner: the native reader may omit an unfinished record, but history
// must never turn that partial result into a completed, reusable snapshot.
test("native history refuses incomplete or changing reads and recovers without stale cached turns", async t => {
  const f = await fixture(t);
  const id = "11111111-2222-4333-8444-555555555555";
  const cwd = await realpath(f.primary);
  const config = join(f.root, "native");
  const files = await nativeFiles(t, config);
  const sdk = await import("@anthropic-ai/claude-agent-sdk");
  const folder = join(config, "projects", projectFolderName(cwd));
  const processes = join(config, "sessions");
  await mkdir(folder, { recursive: true });
  await mkdir(processes);
  const path = join(folder, `${id}.jsonl`);
  const common = { sessionId: id, cwd, isSidechain: false, timestamp: "2026-10-13T10:00:00Z" };
  const rows = [
    { type: "user", uuid: "u1", parentUuid: null, message: { role: "user", content: "First" } },
    { type: "assistant", uuid: "a1", parentUuid: "u1", message: { role: "assistant", id: "m1", stop_reason: "end_turn", content: [{ type: "text", text: "Done" }] } },
  ].map(row => ({ ...common, ...row }));
  const encode = values => values.map(row => JSON.stringify(row)).join("\n") + "\n";
  await writeFile(path, encode(rows));
  let reads = 0, race = null;
  const reader = { getSessionMessages: async (...args) => {
    reads++;
    const messages = await sdk.getSessionMessages(...args);
    if (race) { const change = race; race = null; await change(); }
    return messages;
  } };
  const bridge = new ClaudeBridge({ updates: { acquire: async () => ({ runtime: { sdk: reader }, release: async () => {} }) },
    sessions: f.sessions, send: async () => {}, claudeSessionsDir: processes });
  bridge.sessionFiles = files;
  const session = await f.sessions.create({ wonderProject: { ...f.project, cwd } }, null, { sdkSessionId: id, sdkStarted: true });
  const history = () => bridge.request("thread/turns/list", { threadId: session.id });
  assert.equal((await history()).data[0].status, "completed");
  await history();
  assert.equal(reads, 1, "unchanged complete history reuses the native read");
  const next = { ...common, type: "user", uuid: "u2", parentUuid: "a1", message: { role: "user", content: "Next" } };
  const encoded = JSON.stringify(next);
  await appendFile(path, encoded.slice(0, -3));
  await writeFile(join(processes, `${process.pid}.json`), JSON.stringify({ pid: process.pid, procStart: processLabel(process.pid), sessionId: id, status: "busy" }));
  const partial = await sdk.getSessionMessages(id, { dir: cwd });
  assert.equal(partial.at(-1).uuid, "a1", "SDK 0.3.283 omits the incomplete newest prompt without a partial flag");
  await assert.rejects(history(), /updating this conversation/);
  assert.equal(bridge.transcripts.has(id), false, "partial evidence cannot remain a reusable snapshot");
  await appendFile(path, encoded.slice(-3) + "\n");
  const running = (await history()).data[0];
  assert.deepEqual([running.id, running.status, running.runningElsewhere], ["u2", "inProgress", true]);
  const reply = { ...common, type: "assistant", uuid: "a2", parentUuid: "u2", message: { role: "assistant", id: "m2", stop_reason: "end_turn",
    content: [{ type: "thinking", thinking: "Check the next step.", signature: "sig" }, { type: "text", text: "Next done" }] } };
  await appendFile(path, "\n"); // Invalidate the stable snapshot before the controlled SDK race.
  // An append during the read is read again, never served from the older read.
  race = () => appendFile(path, JSON.stringify(reply) + "\n");
  const readsBefore = reads;
  const finished = (await history()).data[0];
  assert.equal(reads, readsBefore + 2);
  assert.deepEqual([finished.id, finished.status, finished.items.at(-1).text], ["u2", "completed", "Next done"]);
  // A session another app holds (here a busy native process) shows its
  // thinking from the transcript the SDK reads, before the reply.
  assert.deepEqual(finished.items.at(-2), { type: "reasoning", id: "u2:m2:thinking:0", status: "completed", summary: ["Check the next step."] });
  const before = await stat(path);
  reply.message.content[1].text = "Next edit";
  await writeFile(path, encode([...rows, next, reply]) + "\n");
  await utimes(path, before.atime, before.mtime);
  assert.equal((await history()).data[0].items.at(-1).text, "Next edit", "same-size rewrites refresh the SDK read too");
  await writeFile(join(processes, `${process.pid}.json`), `{"pid":${process.pid},"sessionId":"${id}"`);
  await assert.rejects(history(), /session state.*couldn't be read/);
  bridge.active.set(session.id, {});
  try { assert.equal((await history()).data[0].status, "inProgress", "Cupola's owned run remains authoritative"); }
  finally { bridge.active.delete(session.id); }
  await rm(join(processes, `${process.pid}.json`));
  assert.equal((await history()).data[0].status, "completed", "a complete empty process scan proves no external writer");
});

// A cursor belongs to one ordered view; loading another view cannot reuse its hash.
test("history cursors keep loaded, unloaded and per-turn pages separate", async t => {
  const f = await fixture(t);
  const id = "11111111-2222-4333-8444-555555555555";
  const messages = Array.from({ length: 3 }, (_, index) => [
    { type: "user", uuid: `u${index}`, message: { content: `Prompt ${index}` } },
    { type: "assistant", uuid: `a${index}`, message: { id: `m${index}`, content: [{ type: "text", text: `Reply ${index}` }] } },
  ]).flat();
  const folder = join(f.root, "claude-projects", projectFolderName(f.primary));
  await mkdir(folder, { recursive: true });
  await writeFile(join(folder, `${id}.jsonl`), messages.map(m => JSON.stringify(m)).join("\n") + "\n");
  const sdk = { getSessionMessages: async () => messages, getSessionInfo: async () => ({ sessionId: id, cwd: f.primary }) };
  const bridge = new ClaudeBridge({ updates: { acquire: async () => ({ runtime: { sdk }, release: async () => {} }) },
    sessions: f.sessions, send: async () => {}, claudeProjectsDir: join(f.root, "claude-projects"), claudeSessionsDir: join(f.root, "processes") });
  const { thread } = await bridge.request("project/session/attach", { sessionId: id, model: "claude:haiku", wonderPolicy: f.policy, wonderProject: f.project });
  const views = [
    ["thread/turns/list", { itemsView: "notLoaded" }], ["thread/turns/list", {}],
    ["thread/items/list", { sortDirection: "asc" }], ["thread/items/list", { sortDirection: "desc" }],
    ["thread/items/list", { turnId: "u1" }], ["thread/items/list", { turnId: "u2" }],
  ];
  const first = await Promise.all(views.map(([method, params]) => bridge.request(method, { threadId: thread.id, ...params, limit: 1 })));
  for (let i = 0; i < views.length; i++) {
    const [method, params] = views[i];
    const all = await bridge.request(method, { threadId: thread.id, ...params });
    const next = await bridge.request(method, { threadId: thread.id, ...params, cursor: first[i].nextCursor });
    assert.deepEqual([...first[i].data, ...next.data], all.data);
    assert.equal(next.nextCursor, null);
    await assert.rejects(bridge.request(method, { threadId: thread.id, ...params, cursor: first[(i + 1) % views.length].nextCursor }), /history changed/);
  }
});

test("turns and agent tasks from before a compaction stay visible", async t => {
  const root = await mkdtemp(join(tmpdir(), "wonder-claude-projects-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const cwd = "/work/My App", id = "11111111-2222-4333-8444-555555555555";
  const folder = join(root, "projects", projectFolderName(cwd));
  await mkdir(join(folder, id, "subagents"), { recursive: true });
  const rows = [
    { type: "user", uuid: "u1", parentUuid: null, message: { role: "user", content: "Research the bug" }, timestamp: "2026-10-01T00:00:00Z" },
    { type: "assistant", uuid: "a1", parentUuid: "u1", message: { id: "m1", content: [{ type: "tool_use", id: "tu-a", name: "Agent", input: { description: "Find it", prompt: "Look", subagent_type: "Explore" } }] } },
    { type: "assistant", uuid: "a2", parentUuid: "a1", message: { id: "m1", content: [{ type: "tool_use", id: "tu-b", name: "Bash", input: { command: "make" } }] } },
    // Parallel results branch off their own tool calls.
    { type: "user", uuid: "r1", parentUuid: "a1", message: { content: [{ type: "tool_result", tool_use_id: "tu-a", content: "Async agent launched successfully. agentId: x1" }] } },
    { type: "user", uuid: "r2", parentUuid: "a2", message: { content: [{ type: "tool_result", tool_use_id: "tu-b", content: "built" }] } },
    { type: "user", uuid: "side", parentUuid: "r2", isSidechain: true, message: { content: "child" } },
    { type: "user", uuid: "meta", parentUuid: "r2", isMeta: true, message: { content: "caveat" } },
    { type: "system", uuid: "b1", parentUuid: null, logicalParentUuid: "meta", subtype: "compact_boundary" },
    { type: "user", uuid: "s1", parentUuid: "b1", isCompactSummary: true, message: { content: "Summary" } },
    { type: "user", uuid: "u2", parentUuid: "s1", message: { role: "user", content: "Continue" } },
  ];
  await writeFile(join(folder, `${id}.jsonl`), rows.map(r => JSON.stringify(r)).join("\n") + "\n");
  await writeFile(join(folder, id, "subagents", "agent-x1.meta.json"), JSON.stringify({ agentType: "Explore", description: "Find it", toolUseId: "tu-a", requestShape: "background" }));
  await writeFile(join(folder, id, "subagents", "agent-x1.jsonl"), [
    { type: "user", uuid: "helper-u1", parentUuid: null, sessionId: id, isSidechain: true, agentId: "x1", timestamp: "2026-10-01T00:00:01Z", message: { role: "user", content: "Look" } },
    { type: "assistant", uuid: "helper-a1", parentUuid: "helper-u1", sessionId: id, isSidechain: true, agentId: "x1", message: { role: "assistant", stop_reason: "end_turn", content: [{ type: "text", text: "Found" }] } }].map(r => JSON.stringify(r)).join("\n") + "\n");
  await writeFile(join(folder, id, "subagents", "agent-..%2f.meta.json"), "{}");
  const files = await nativeFiles(t, root);
  const earlier = await files.earlierMessages(id, cwd, "s1");
  assert.deepEqual(earlier.map(m => m.uuid), ["u1", "a1", "a2", "r1", "r2", "b1"]);
  const recent = [{ type: "user", uuid: "s1", isCompactSummary: true, is_meta: true, message: { content: "Summary" } },
    { type: "user", uuid: "u2", message: { content: "Continue" } }];
  assert.deepEqual(nativeTurns([...earlier, ...recent]).map(t => t.id), ["u1", "u2"]);
  const agents = await files.agentTasks(id, cwd);
  assert.deepEqual(agents.map(a => [a.agentId, a.finished, a.startedAt]), [["x1", true, "2026-10-01T00:00:01Z"]]);
  // The launch acknowledgement neither completes the agent nor becomes its result.
  const live = backgroundTasks(earlier, true, [{ ...agents[0], finished: false }]);
  assert.deepEqual(live.map(t => [t.id, t.kind, t.status, t.role, t.taskId]), [["tu-a", "agent", "running", "Explore", "x1"]], "a foreground Bash run is not a task");
  assert.equal(live[0].result, null);
  assert.equal(backgroundTasks([], false, agents)[0].status, "completed");
  // Appending reads only the new line and keeps the parsed history.
  await writeFile(join(folder, `${id}.jsonl`), rows.concat([{ type: "user", uuid: "u3", parentUuid: "u2", message: { content: "More" } }]).map(r => JSON.stringify(r)).join("\n") + "\n");
  assert.deepEqual((await files.earlierMessages(id, cwd, "u3")).map(m => m.uuid), ["u1", "a1", "a2", "r1", "r2", "b1", "u2"]);
  assert.deepEqual(await files.earlierMessages("not-a-uuid", cwd, "u2"), []);
});

// Requests read one transcript concurrently; each appended line is parsed
// into the shared incremental cache exactly once.
test("concurrent transcript reads parse each appended line once", async t => {
  const root = await mkdtemp(join(tmpdir(), "wonder-claude-concurrent-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const path = join(root, "session.jsonl");
  await writeFile(path, JSON.stringify({ type: "user", uuid: "one" }) + "\n");
  const files = new ClaudeSessionFiles(root);
  await files.entries(path);
  await appendFile(path, JSON.stringify({ type: "user", uuid: "two" }) + "\n");
  const [first, second] = await Promise.all([files.entries(path), files.entries(path)]);
  assert.equal(first, second);
  assert.deepEqual(second.rows.map(row => row.uuid), ["one", "two"]);
  assert.equal(second.offset, (await stat(path)).size);
  // A native rewrite of the same length must refresh supplemental records,
  // including when a copy tool preserves the original modification time.
  const before = await stat(path);
  await writeFile(path, ["new", "end"].map(uuid => JSON.stringify({ type: "user", uuid })).join("\n") + "\n");
  await utimes(path, before.atime, before.mtime);
  assert.deepEqual((await files.entries(path)).rows.map(row => row.uuid), ["new", "end"]);

  // Native appends may end inside any multi-byte character. Publish only
  // the complete JSON line, retaining its text exactly across concurrent reads.
  const message = { type: "user", uuid: "unicode", message: { content: "café 語 🧭" } };
  for (const byte of Buffer.from(JSON.stringify(message))) {
    await appendFile(path, Buffer.from([byte]));
    const [a, b] = await Promise.all([files.entries(path), files.entries(path)]);
    assert.equal(a, b);
    assert.equal(a.rows.length, 2);
  }
  await appendFile(path, "\n");
  const complete = await files.entries(path);
  assert.equal(complete.rows.length, 3);
  assert.deepEqual((await files.fullRows(path, complete, [complete.rows[2]]))[0].message, message.message);
  assert.equal(complete.offset, (await stat(path)).size);

  // Discard a pending character along with the old file when it is rewritten.
  await appendFile(path, Buffer.from("🧭").subarray(0, 2));
  await files.entries(path);
  await writeFile(path, JSON.stringify(message) + "\n");
  const rewritten = await files.entries(path);
  assert.deepEqual(rewritten.rows.map(row => row.uuid), ["unicode"]);
  assert.deepEqual((await files.fullRows(path, rewritten, rewritten.rows))[0].message, message.message);
});

// The native reader handles large records and resumed helpers. Unchanged
// revisions reuse the prepared tasks without reading the transcript again.
test("agent task polling uses native messages and rereads only changed helper files", async t => {
  const root = await mkdtemp(join(tmpdir(), "wonder-claude-tasks-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const cwd = "/work/Tasks", id = "21111111-2222-4333-8444-555555555555";
  const folder = join(root, "projects", projectFolderName(cwd)), dir = join(folder, id, "subagents");
  await mkdir(dir, { recursive: true });
  const main = join(folder, `${id}.jsonl`);
  await writeFile(main, JSON.stringify({ type: "user", uuid: "u1", message: { content: "Go" } }) + "\n");
  const helper = async (agentId, description, finished) => {
    await writeFile(join(dir, `agent-${agentId}.meta.json`), JSON.stringify({ agentType: "Explore", description, toolUseId: `tu-${agentId}`, requestShape: "background" }));
    await writeFile(join(dir, `agent-${agentId}.jsonl`), [
      { type: "user", uuid: "helper-prompt", parentUuid: null, sessionId: id, isSidechain: true, agentId, timestamp: "2026-10-01T00:00:01Z", message: { role: "user", content: "Look" } },
      ...(finished ? [{ type: "assistant", uuid: "helper-end", parentUuid: "helper-prompt", sessionId: id, isSidechain: true, agentId, message: { role: "assistant", stop_reason: "end_turn", content: [{ type: "text", text: "Finished" }] } }] : [])].map(r => JSON.stringify(r)).join("\n") + "\n");
  };
  await helper("done", "Finished one", true);
  await helper("busy", "Running one", false);
  const files = await nativeFiles(t, root);
  const native = files.withSdk;
  files.withSdk = action => native(sdk => action({ ...sdk, getSubagentMessages: async () => { throw new Error("A task badge must not materialize helper SDK history"); } }));
  const read = async () => Object.fromEntries((await files.agentTasks(id, cwd)).map(a => [a.agentId, [a.description, a.finished]]));
  assert.deepEqual(await read(), { done: ["Finished one", true], busy: ["Running one", false] });
  const cachedTasks = await files.agentTasks(id, cwd);
  assert.equal(await files.agentTasks(id, cwd), cachedTasks);
  // Even an in-place rewrite preserving mtime invalidates through ctime.
  const meta = join(dir, "agent-done.meta.json"), before = await stat(meta);
  await writeFile(meta, JSON.stringify({ agentType: "Explore", description: "Modified one", toolUseId: "tu-done", requestShape: "background" }));
  await utimes(meta, before.atime, before.mtime);
  assert.deepEqual(await read(), { done: ["Modified one", true], busy: ["Running one", false] });
  // Current action shares that same bounded read/cache, including parallel
  // calls, their exact results, a partial newest line and the terminal reply.
  const busyPath = join(dir, "agent-busy.jsonl");
  const tool = (id, name, input) => ({ type: "assistant", message: { content: [{ type: "tool_use", id, name, input }] } });
  let sequence = 0, parent = "helper-prompt";
  const append = async row => {
    const uuid = `helper-${++sequence}`;
    await appendFile(busyPath, JSON.stringify({ ...row, uuid, parentUuid: parent, sessionId: id, agentId: "busy", isSidechain: true }) + "\n");
    parent = uuid;
  };
  const step = async () => (await files.agentTasks(id, cwd)).find(a => a.agentId === "busy").currentActivity;
  await append(tool("read", "Read", { file_path: "/work/AppShell.swift", fixturePadding: "x".repeat(100 * 1024) }));
  assert.equal((await step()).toolAction.kind, "read");
  assert.equal(backgroundTasks([], true, await files.agentTasks(id, cwd)).find(task => task.id === "tu-busy").currentActivity.id, "read");
  await append(tool("search", "Grep", { pattern: "isWorking" }));
  assert.equal((await step()).toolAction.query, "isWorking");
  await append({ type: "user", message: { content: [{ type: "tool_result", tool_use_id: "search", content: "done" }] } });
  assert.equal((await step()).id, "read");
  await appendFile(busyPath, `{"type":"assistant","uuid":"partial","parentUuid":"${parent}"`);
  assert.equal(await step(), null, "Partial newest evidence cannot revive an earlier action.");
  await appendFile(busyPath, ',"message":{"stop_reason":"end_turn","content":[{"type":"text","text":"Done"}]}}\n');
  parent = "partial";
  assert.equal(await step(), null);
  // The running helper finishes: picked up without a change to the conversation.
  await append({ type: "assistant", message: { stop_reason: "end_turn", content: [{ type: "text", text: "Done" }] } });
  assert.deepEqual((await read()).busy, ["Running one", true]);
  // Native discovery sees a resumed/new helper without requiring a parent
  // transcript write first. A finished helper's changed revision is re-read.
  await append({ type: "user", message: { content: "Continue looking" } });
  await append(tool("resumed", "Read", { file_path: "/work/Next.swift" }));
  assert.equal((await step()).id, "resumed");
  assert.equal((await read()).busy[1], false);
  // A new helper is discovered from the SDK's native directory listing.
  await helper("next", "Next one", false);
  assert.deepEqual((await read()).next, ["Next one", false]);
  // A changed revision is read again.
  await writeFile(meta, JSON.stringify({ agentType: "Explore", description: "Renamed helper", toolUseId: "tu-done", requestShape: "background" }));
  await appendFile(main, JSON.stringify({ type: "user", uuid: "u2", message: { content: "More" } }) + "\n");
  assert.deepEqual((await read()).done, ["Renamed helper", true]);
});

// A slow SDK lease/read must not accumulate work on each phone poll. This
// delays the real reader's acquisition; it does not emulate native messages.
test("agent task polling bounds callers while retaining one outstanding SDK read", async t => {
  const root = await mkdtemp(join(tmpdir(), "wonder-claude-task-timeout-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const files = await nativeFiles(t, root), native = files.withSdk;
  const id = "31111111-2222-4333-8444-555555555555";
  let release, completed, acquisitions = 0;
  const gate = new Promise(resolve => { release = resolve; });
  const ended = new Promise(resolve => { completed = resolve; });
  files.withSdk = async action => {
    acquisitions++;
    try { await gate; return await native(action); } finally { completed(); }
  };
  t.after(() => release());
  const [, tasks] = await Promise.all([
    assert.rejects(files.agentTasks(id, "/work/Tasks", { requireComplete: true }), /still reading task status/),
    Promise.all([files.agentTasks(id, "/work/Tasks"), files.agentTasks(id, "/work/Tasks")]),
  ]);
  assert.deepEqual(tasks, [[], []]);
  assert.equal(acquisitions, 1);
  release(); await ended;
  assert.deepEqual(await files.agentTasks(id, "/work/Tasks"), []);
});

// A slow native listing or helper must not restart the same work forever.
// Delay real SDK reads past the caller deadline; never publish a partial roster.
test("agent task polling retains verified progress across slow native reads", async t => {
  const root = await mkdtemp(join(tmpdir(), "wonder-claude-task-progress-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const cwd = "/work/TaskProgress", id = "41111111-2222-4333-8444-555555555555";
  const folder = join(root, "projects", projectFolderName(cwd)), dir = join(folder, id, "subagents");
  await mkdir(dir, { recursive: true });
  await writeFile(join(folder, `${id}.jsonl`), JSON.stringify({ type: "user", uuid: "u1", message: { content: "Look" } }) + "\n");
  for (const agentId of ["alpha", "beta"]) {
    await writeFile(join(dir, `agent-${agentId}.meta.json`), JSON.stringify({ agentType: "Explore", description: agentId, toolUseId: `tu-${agentId}`, requestShape: "background" }));
    await writeFile(join(dir, `agent-${agentId}.jsonl`), [
      { type: "user", uuid: "u1", parentUuid: null, sessionId: id, isSidechain: true, agentId, message: { role: "user", content: "Look" } },
      { type: "assistant", uuid: "a1", parentUuid: "u1", sessionId: id, isSidechain: true, agentId, message: { role: "assistant", stop_reason: "end_turn", content: [{ type: "text", text: "Finished" }] } }
    ].map(row => JSON.stringify(row)).join("\n") + "\n");
  }
  const files = await nativeFiles(t, root), native = files.withSdk, entries = files.entries.bind(files);
  let pending, lists = 0, messages = 0;
  const delay = () => new Promise(resolve => setTimeout(resolve, 1200));
  files.withSdk = action => pending = native(sdk => action({ ...sdk,
    listSubagents: async (...args) => { lists++; const rows = await sdk.listSubagents(...args); await delay(); return rows; },
    getSubagentMessages: async () => { throw new Error("Task status never reads SDK helper history"); }
  }));
  files.entries = async (...args) => { messages++; const rows = await entries(...args); await delay(); return rows; };
  for (let poll = 0; poll < 3; poll++) {
    assert.deepEqual(await files.agentTasks(id, cwd), []);
    await pending;
    await new Promise(resolve => setImmediate(resolve));
  }
  const tasks = await files.agentTasks(id, cwd);
  assert.deepEqual(tasks.map(task => [task.agentId, task.finished]).sort(), [["alpha", true], ["beta", true]]);
  assert.deepEqual({ lists, messages }, { lists: 1, messages: 2 }, "unchanged native results are read once");
  await rm(join(dir, "agent-beta.meta.json"));
  await rm(join(dir, "agent-beta.jsonl"));
  assert.deepEqual(await files.agentTasks(id, cwd), []);
  await pending;
  await new Promise(resolve => setImmediate(resolve));
  assert.deepEqual((await files.agentTasks(id, cwd)).map(task => task.agentId), ["alpha"]);
  assert.deepEqual({ lists, messages }, { lists: 2, messages: 2 }, "native inventory changes remove cached tasks without rereading unchanged helpers");
});

// Shape recorded by Claude Code: a resumed background agent notifies again under
// the resuming call's tool-use ID but keeps its task ID.
test("a resumed agent's later notice updates its task instead of adding a command row", () => {
  const agentNotice = (id, toolUseId, status, summary) => ({ type: "user", uuid: id,
    message: { content: `<task-notification>\n<task-id>a0b1c2</task-id>\n<tool-use-id>${toolUseId}</tool-use-id>\n<status>${status}</status>\n<summary>${summary}</summary>\n</task-notification>` } });
  const messages = [
    { type: "user", uuid: "turn-1", message: { content: "Go" } },
    { type: "assistant", uuid: "a1", timestamp: "2026-10-06T20:12:08Z", message: { id: "m1", content: [
      { type: "tool_use", id: "tu-agent", name: "Agent", input: { description: "Readiness pass", subagent_type: "general-purpose", prompt: "Do it", run_in_background: true } },
      { type: "tool_use", id: "tu-bg", name: "Bash", input: { command: "make build\nmake test", run_in_background: true } }] } },
    { type: "user", uuid: "r1", message: { content: [{ type: "tool_result", tool_use_id: "tu-agent",
      content: [{ type: "text", text: "Async agent launched successfully.\nagentId: a0b1c2 (internal ID)" }] }] } },
    agentNotice("n1", "tu-agent", "completed", 'Agent "Readiness pass" finished'),
    { type: "assistant", uuid: "a2", message: { id: "m2", content: [{ type: "tool_use", id: "tu-resume", name: "SendMessage", input: { to: "a0b1c2" } }] } },
    agentNotice("n2", "tu-resume", "failed", 'Agent "Readiness pass" failed: API error'),
  ];
  const tasks = backgroundTasks(messages, true);
  assert.deepEqual(tasks.map(t => [t.id, t.kind, t.title, t.status]), [
    ["tu-bg", "command", "make build", "running"], ["tu-agent", "agent", "Readiness pass", "failed"]]);
  // Without the launch in view, a notice that names an agent still makes an agent row.
  assert.equal(backgroundTasks([agentNotice("n3", "tu-gone", "completed", 'Agent "Old pass" finished')], true)[0].kind, "agent");
});

test("a running agent's unanswered tool call is in progress, answered ones are done", () => {
  const [turn] = nativeTurns([
    { type: "user", uuid: "p", message: { content: "Look" } },
    { type: "assistant", uuid: "a1", message: { id: "m1", content: [
      { type: "tool_use", id: "t1", name: "Bash", input: { command: "ls" } },
      { type: "tool_use", id: "t2", name: "Grep", input: { pattern: "x" } }] } },
    { type: "user", uuid: "r1", message: { content: [{ type: "tool_result", tool_use_id: "t1", content: "ok" }] } },
  ], [], true);
  assert.deepEqual(turn.items.slice(1).map(i => [i.id, i.status]), [["t1", "completed"], ["t2", "inProgress"]]);
  const [done] = nativeTurns([
    { type: "user", uuid: "p", message: { content: "Look" } },
    { type: "assistant", uuid: "a1", message: { id: "m1", content: [{ type: "tool_use", id: "t2", name: "Grep", input: {} }] } },
  ], [], false);
  assert.equal(done.items[1].status, "unknown", "An idle transcript without a result does not prove tool completion");
});

test("Latest settings come from the newest assistant message and say who wrote it", async t => {
  const root = await mkdtemp(join(tmpdir(), "wonder-settings-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const cwd = join(root, "app"), id = "11111111-2222-3333-4444-555555555555";
  const folder = join(root, projectFolderName(cwd));
  await mkdir(folder, { recursive: true });
  const assistant = (uuid, extra) => ({ type: "assistant", uuid, message: { model: "claude-opus-5-5", usage: { speed: "standard" } }, ...extra });
  const write = rows => writeFile(join(folder, `${id}.jsonl`), rows.map(r => JSON.stringify(r)).join("\n") + "\n");
  await write([assistant("a1", { entrypoint: "claude-desktop", effort: "xhigh" }),
    { type: "assistant", uuid: "a2", message: { model: "<synthetic>" }, entrypoint: "claude-desktop" },
    assistant("side", { isSidechain: true, effort: "low" })]);
  const files = new ClaudeSessionFiles(root);
  assert.deepEqual(await files.latestSettings(id, cwd), { turnId: "a1", native: true, model: "claude-opus-5-5", effort: "xhigh", speed: "standard" });
  await write([assistant("a1", { entrypoint: "claude-desktop", effort: "xhigh" }), assistant("a3", { entrypoint: "sdk-ts" })]);
  assert.deepEqual(await files.latestSettings(id, cwd), { turnId: "a3", native: false, model: "claude-opus-5-5", effort: null, speed: "standard" });
  assert.equal(await files.latestSettings("not-a-uuid", cwd), null);
});

const forkMessages = [
  { type: "user", uuid: "turn-1", origin: { kind: "human" }, message: { content: "First ask" } },
  { type: "assistant", uuid: "a1", message: { id: "m1", content: [{ type: "tool_use", id: "tu1", name: "Bash", input: { command: "ls" } }] } },
  { type: "user", uuid: "r1", message: { content: [{ type: "tool_result", tool_use_id: "tu1", content: "files" }] } },
  { type: "assistant", uuid: "a2", message: { id: "m2", content: [{ type: "text", text: "Done one" }] } },
  { type: "user", uuid: "turn-2", origin: { kind: "human" }, message: { content: "Second ask" } },
  { type: "assistant", uuid: "a3", message: { id: "m3", content: [{ type: "text", text: "Done two" }] } },
];

test("a fork ends at the last entry of the chosen turn, never inside the next one", () => {
  assert.deepEqual(nativeTurns(forkMessages).map(t => t.id), ["turn-1", "turn-2"]);
  assert.equal(turnEndMessageId(forkMessages, "turn-1"), "a2");
  assert.equal(turnEndMessageId(forkMessages, "turn-2"), "a3");
  assert.equal(turnEndMessageId(forkMessages, "r1"), null, "a tool result does not open a turn");
  assert.equal(turnEndMessageId(forkMessages, "missing"), null);
});

// Contract: the native catalog decorates only positive producer evidence. A
// native ID, a later desktop turn, or copied fork history never proves origin.
test("catalog computer provenance uses the original root entrypoint and rejects copied forks", async t => {
  const root = await mkdtemp(join(tmpdir(), "wonder-origin-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const cwd = join(root, "work"), folder = join(root, "projects", projectFolderName(cwd));
  await mkdir(folder, { recursive: true });
  const files = await nativeFiles(t, root);
  for (const [index, entrypoint, extra, expected] of [
    [1, "cli", {}, true], [2, "claude-desktop", {}, true], [3, "sdk-ts", {}, false],
    [4, undefined, {}, undefined], [5, "future-client", {}, undefined],
    [6, "cli", { forkedFrom: { sessionId: "copied", messageUuid: "old" } }, undefined],
    [7, "cli", { parentUuid: "missing-root" }, undefined],
    [8, "cli", { sessionId: "different" }, undefined],
    [9, "cli", { isCompactSummary: true }, undefined],
    [10, "cli", { sessionKind: "daemon" }, undefined],
  ]) {
    const sessionId = `11111111-2222-4333-8444-${String(index).padStart(12, "0")}`;
    const first = { type: "user", uuid: `u${index}`, parentUuid: null, sessionId, cwd, entrypoint,
      timestamp: "2026-10-16T12:00:00Z", message: { role: "user", content: "Synthetic origin fixture" }, ...extra };
    await writeFile(join(folder, `${sessionId}.jsonl`), [first,
      { ...first, uuid: `a${index}`, type: "assistant", parentUuid: first.uuid, entrypoint: "claude-desktop",
        message: { role: "assistant", content: [{ type: "text", text: "Synthetic reply" }] } }].map(JSON.stringify).join("\n") + "\n");
    const page = await files.listSessions({ dirs: [cwd], sessionId });
    assert.equal(page.data[0]?.isComputerSession, expected, `original entrypoint ${entrypoint}, case ${index}`);
  }
  // SDK forkSession rewrites session IDs and UUIDs but preserves entrypoints:
  // its per-message forkedFrom is essential, even when Cupola's insert failed.
  const original = "11111111-2222-4333-8444-000000000001";
  const sdk = await import("@anthropic-ai/claude-agent-sdk");
  const fork = await sdk.forkSession(original, { dir: cwd });
  const forked = await files.listSessions({ dirs: [cwd], sessionId: fork.sessionId });
  assert.equal(forked.data[0]?.isComputerSession, undefined);
  const page = await files.listSessions({ dirs: [cwd], limit: 100 });
  assert.deepEqual(page.data.filter(row => row.isComputerSession).map(row => row.sessionId).sort(),
    [original, "11111111-2222-4333-8444-000000000002"]);

  // A Wonder/SDK-origin session can now be held by an interactive process.
  // Its PID birth time, entrypoint and known state must all be affirmative.
  const sdkSession = "11111111-2222-4333-8444-000000000003";
  const records = join(root, "sessions"), record = join(records, `${process.pid}.json`);
  await mkdir(records);
  for (const [entrypoint, procStart, status, expected] of [
    ["cli", processLabel(process.pid), "idle", true],
    ["claude-desktop", processLabel(process.pid), "busy", true],
    ["sdk-ts", processLabel(process.pid), "idle", false],
    ["future-client", processLabel(process.pid), "busy", false],
    ["cli", "recycled PID", "idle", false],
    ["cli", processLabel(process.pid), "unknown", false],
  ]) {
    await writeFile(record, JSON.stringify({ pid: process.pid, procStart, sessionId: sdkSession, entrypoint, status }));
    assert.equal((await files.listSessions({ dirs: [cwd], sessionId: sdkSession })).data[0].isComputerSession, expected,
      `${entrypoint}, ${status}, valid birth time ${procStart !== "recycled PID"}`);
  }
  await rm(record);
  assert.equal((await files.listSessions({ dirs: [cwd], sessionId: sdkSession })).data[0].isComputerSession, false);
});

test("catalog origin reads are bounded and never follow a transcript symlink", async t => {
  const root = await mkdtemp(join(tmpdir(), "wonder-origin-bounds-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const cwd = join(root, "work"), folder = join(root, "projects", projectFolderName(cwd));
  await mkdir(folder, { recursive: true });
  const files = new ClaudeSessionFiles(join(root, "projects"));
  const id = "11111111-2222-4333-8444-555555555555", path = join(folder, `${id}.jsonl`);
  const row = JSON.stringify({ type: "user", uuid: "u", parentUuid: null, sessionId: id, entrypoint: "cli" }) + "\n";
  await writeFile(path, " ".repeat(64 * 1024) + "\n" + row);
  assert.equal(await files.startedOnComputer(id, cwd), undefined, "no evidence beyond the prefix budget");
  await writeFile(path, row.slice(0, -1));
  assert.equal(await files.startedOnComputer(id, cwd), undefined, "an incomplete root line is unknown");
  await rm(path);
  const outside = join(root, "outside.jsonl"); await writeFile(outside, row); await symlink(outside, path);
  assert.equal(await files.startedOnComputer(id, cwd), undefined, "a substituted transcript is unknown");
  assert.equal(await files.startedOnComputer("../outside", cwd), undefined);
});

test("forking copies the native session through the chosen turn and leaves the source alone", async t => {
  const f = await fixture(t);
  const id = "11111111-2222-4333-8444-555555555555", forked = [];
  const claudeSessionsDir = await mkdtemp(join(tmpdir(), "wonder-claude-sessions-"));
  t.after(() => rm(claudeSessionsDir, { recursive: true, force: true }));
  const sdk = { getSessionMessages: async () => forkMessages, getSessionInfo: async (sessionId, { dir }) => ({ sessionId, cwd: dir, lastModified: 1 }),
    forkSession: async (sessionId, options) => { forked.push({ sessionId, options }); return { sessionId: "99999999-8888-4777-8666-555555555555" }; } };
  const bridge = new ClaudeBridge({ updates: { acquire: async () => ({ runtime: { sdk }, release: async () => {} }) },
    sessions: f.sessions, send: async () => {}, claudeSessionsDir });
  const { thread } = await bridge.request("project/session/attach", { sessionId: id, model: "claude:haiku", wonderPolicy: f.policy, wonderProject: f.project });

  const first = await bridge.request("project/session/fork", { threadId: thread.id, lastTurnId: "turn-1", title: "Plan (fork)" });
  assert.equal(first.sessionId, "99999999-8888-4777-8666-555555555555");
  assert.deepEqual(forked[0], { sessionId: id, options: { dir: f.primary, upToMessageId: "a2", title: "Plan (fork)" } });
  await bridge.request("project/session/fork", { threadId: thread.id });
  assert.deepEqual(forked[1].options, { dir: f.primary }, "the latest turn copies the whole session");
  await assert.rejects(bridge.request("project/session/fork", { threadId: thread.id, lastTurnId: "gone" }), /no longer in this conversation/);
  assert.equal(forked.length, 2);
  assert.equal(f.sessions.get(thread.id).sdkSessionId, id, "the source keeps its session");

  const nativeRecord = join(claudeSessionsDir, `${process.pid}.json`);
  for (const record of [
    { pid: process.pid, procStart: processLabel(process.pid), sessionId: id },
    { pid: process.pid, procStart: "another process", sessionId: id, status: "idle" },
    { pid: process.pid, procStart: processLabel(process.pid), sessionId: id, status: "future-state" },
    `{"pid":${process.pid},"sessionId":"${id}"`, JSON.stringify({ pid: process.pid, sessionId: id, padding: "x".repeat(64 * 1024) }),
  ]) {
    await writeFile(nativeRecord, typeof record === "string" ? record : JSON.stringify(record));
    await assert.rejects(bridge.request("project/session/fork", { threadId: thread.id }), /session state on your computer couldn't be read/);
    await assert.rejects(bridge.request("turn/start", { threadId: thread.id, input: [{ type: "text", text: "Fixture" }] }), error => {
      assert.match(error.message, /open in Claude on your computer|session state on your computer couldn't be read/);
      assert.equal(error.method, "turn/start"); assert.equal(error.action, "wait"); return true;
    });
    assert.equal(forked.length, 2, "unknown native state never invokes forkSession");
  }
  await rm(nativeRecord);
  await symlink(join(claudeSessionsDir, "missing"), nativeRecord);
  assert.equal((await claudeSessionState(id, claudeSessionsDir)).open, false, "a symlink is never followed and names no chat");
  await rm(nativeRecord);
  if (process.platform !== "win32") { // Windows has no FIFOs.
    execFileSync("/usr/bin/mkfifo", [nativeRecord]);
    const started = performance.now();
    assert.equal((await claudeSessionState(id, claudeSessionsDir)).open, false, "a FIFO is never read and names no chat");
    assert.ok(performance.now() - started < 1000, "a FIFO cannot block admission");
    await rm(nativeRecord);
  }

  bridge.active.set(thread.id, {});
  await assert.rejects(bridge.request("project/session/fork", { threadId: thread.id }), /Wait for Claude to finish/);
  bridge.active.delete(thread.id);
  const draft = await bridge.request("thread/start", { cwd: f.primary, model: "claude:haiku", wonderPolicy: f.policy, wonderProject: f.project });
  await assert.rejects(bridge.request("project/session/fork", { threadId: draft.thread.id }), /Send a message first/);
});

// Contract: Cupola stops an agent task or background command only while it
// drives the session, through Claude Code's own stop_task control. A session
// running in Claude on the Mac keeps its own controls.
test("stopping an agent task or background command needs a session Cupola drives", async t => {
  const f = await fixture(t);
  const dir = await mkdtemp(join(tmpdir(), "wonder-claude-sessions-"));
  t.after(() => rm(dir, { recursive: true, force: true }));
  const sdkSessionId = "11111111-2222-3333-4444-555555555555";
  const messages = [
    { type: "user", uuid: "turn-1", origin: { kind: "human" }, message: { content: "Go" } },
    // Written by the process the test started, so after it started.
  { type: "assistant", uuid: "a1", timestamp: new Date().toISOString(), message: { id: "m1", content: [
      { type: "tool_use", id: "tu-agent", name: "Agent", input: { description: "Audit", subagent_type: "Explore", prompt: "Look", run_in_background: true } },
      { type: "tool_use", id: "tu-cmd", name: "Bash", input: { command: "make watch", run_in_background: true } },
      { type: "tool_use", id: "tu-done", name: "Bash", input: { command: "make", run_in_background: true } }] } },
    { type: "user", uuid: "r1", message: { content: [
      { type: "tool_result", tool_use_id: "tu-agent", content: [{ type: "text", text: "Async agent launched successfully.\nagentId: a0b1c2" }] },
      { type: "tool_result", tool_use_id: "tu-cmd", content: "Command running in background with ID: bwatch1. Output is being written to: /tmp/x" },
      { type: "tool_result", tool_use_id: "tu-done", content: "Command running in background with ID: bdone1." }] } },
    { type: "user", uuid: "n1", message: { content: "<task-notification>\n<task-id>bdone1</task-id>\n<tool-use-id>tu-done</tool-use-id>\n<status>completed</status>\n<summary>done</summary>\n</task-notification>" } },
  ];
  const sdk = { getSessionMessages: async () => messages, getSessionInfo: async (id, { dir: cwd }) => ({ sessionId: id, cwd, lastModified: 1 }) };
  const bridge = new ClaudeBridge({ updates: { acquire: async () => ({ runtime: { sdk }, release: async () => {} }) },
    sessions: f.sessions, send: async () => {}, claudeSessionsDir: dir });
  syntheticTaskFiles(bridge, () => messages);
  t.after(() => bridge.close());
  const { thread } = await bridge.request("project/session/attach", { sessionId: sdkSessionId, model: "claude:haiku", wonderPolicy: f.policy, wonderProject: f.project });
  const list = async () => Object.fromEntries((await bridge.request("thread/backgroundTasks/list", { threadId: thread.id })).data.map(task => [task.id, task]));
  const stop = taskId => bridge.request("thread/backgroundTask/stop", { threadId: thread.id, taskId });

  // Nothing is running without a live session: states are unknown and Stop is refused.
  assert.equal((await list())["tu-cmd"].canStop, false);
  assert.equal((await stop("tu-cmd")).outcome, "notRunning");

  // Running in Claude on the Mac: explain instead of stopping.
  await writeFile(join(dir, `${process.pid}.json`), JSON.stringify({ pid: process.pid, procStart: processLabel(process.pid), sessionId: sdkSessionId, status: "busy" }));
  const elsewhere = await list();
  assert.deepEqual([elsewhere["tu-cmd"].status, elsewhere["tu-cmd"].canStop, elsewhere["tu-cmd"].runningElsewhere], ["running", false, true]);
  assert.equal((await stop("tu-cmd")).outcome, "runningElsewhere");
  await writeFile(join(dir, `${process.pid}.json`), JSON.stringify({ pid: process.pid, procStart: processLabel(process.pid), sessionId: sdkSessionId, status: "idle" }));

  // Cupola drives the session: running tasks with a task ID can stop; finished ones cannot.
  const stopped = [];
  bridge.active.set(thread.id, { turn: { id: "t" }, query: { stopTask: async id => { stopped.push(id); } } });
  const tasks = await list();
  assert.deepEqual(["tu-agent", "tu-cmd", "tu-done"].map(id => [tasks[id].status, tasks[id].canStop]),
    [["running", true], ["running", true], ["completed", false]]);
  assert.ok(!("taskId" in tasks["tu-cmd"]), "SDK task IDs stay inside the bridge");
  assert.equal((await stop("tu-done")).outcome, "notRunning");
  assert.equal((await stop("nope")).outcome, "notFound");
  assert.deepEqual(stopped, []);
  const result = await stop("tu-agent");
  assert.deepEqual(stopped, ["a0b1c2"]);
  assert.deepEqual([result.outcome, result.task.status], ["stopped", "interrupted"]);
  assert.equal((await list())["tu-agent"].status, "interrupted");
  await stop("tu-cmd");
  assert.deepEqual(stopped, ["a0b1c2", "bwatch1"]);
  assert.equal((await stop("tu-cmd")).outcome, "notRunning");
  assert.deepEqual(stopped, ["a0b1c2", "bwatch1"]);
  bridge.active.delete(thread.id);
});

// Like Claude Desktop, interrupting a reply ends the reply but spares running
// agents; the user stops each from the task list, and the next message joins
// the same Claude process instead of ending it.
const agentTranscript = (...extra) => [
  { type: "user", uuid: "turn-1", origin: { kind: "human" }, message: { content: "Go" } },
  // Written by the process the test started, so after it started.
  { type: "assistant", uuid: "a1", timestamp: new Date().toISOString(), message: { id: "m1", content: [
    { type: "tool_use", id: "tu-agent", name: "Agent", input: { description: "Audit", subagent_type: "Explore", prompt: "Look", run_in_background: true } }] } },
  { type: "user", uuid: "r1", message: { content: [
    { type: "tool_result", tool_use_id: "tu-agent", content: [{ type: "text", text: "Async agent launched successfully.\nagentId: a0b1c2" }] }] } },
  { type: "user", uuid: "stop", message: { content: [{ type: "text", text: "[Request interrupted by user]" }] } },
  ...extra];
const settle = async (check) => { for (let i = 0; i < 400 && !check(); i++) await new Promise(resolve => setTimeout(resolve, 5)); };

test("a second distinct owner Stop kills a stuck reply; replay and automatic Stop do not", { timeout: 10000 }, async t => {
  const contract = JSON.parse(await readFile(new URL("../../../tests/contracts/native/stop-actions.json", import.meta.url)));
  const f = await fixture(t);
  f.gate.current = Promise.withResolvers();
  const { thread } = await f.bridge.request("thread/start", { cwd: f.primary, model: "claude:haiku", wonderPolicy: f.policy, wonderProject: f.project });
  const { turn } = await f.bridge.request("turn/start", { threadId: thread.id, input: [{ type: "text", text: "Wait for the tool" }] });
  await settle(() => f.inputs.length);
  const run = f.bridge.active.get(thread.id);
  // The SDK accepts interrupt but never produces its result or ends the tool wait.
  run.query.interrupt = async () => { f.interrupts.push(1); };
  const tool = f.bridge.serverCall("item/tool/call", { threadId: thread.id, turnId: turn.id, callId: "stuck-tool" }, run.abort.signal);
  const cancelled = assert.rejects(tool, /cancelled/);
  const stop = body => f.bridge.request("turn/interrupt", { threadId: thread.id, turnId: turn.id, ...body });
  await stop(contract.first);
  await stop(contract.first);
  await stop(contract.legacy);
  assert.equal(f.closes.length, 0, "replay/automatic handoff cannot kill the query");
  assert.equal(f.bridge.pending.size, 1, "the first Stop leaves the owned tool wait pending");
  await stop(contract.second);
  assert.ok(f.closes.length > 0, "a second owner action must close the stuck query");
  await run.finished;
  await cancelled;
  assert.equal(f.bridge.pending.size, 0, "escalation cancels the owned tool wait");
  assert.equal(f.interrupts.length, 1);
  assert.equal(f.sessions.get(thread.id).turns.at(-1).status, "interrupted");
  assert.equal(f.bridge.active.has(thread.id), false);
  assert.equal(f.bridge.lingering.has(thread.id), false);
  assert.equal(f.frames.filter(frame => frame.method === "turn/completed" && frame.params.turn.id === turn.id).length, 1);
});

test("owner Stops can escalate while interrupt acknowledgement or driver Stop is pending", { timeout: 20000 }, async t => {
  const contract = JSON.parse(await readFile(new URL("../../../tests/contracts/native/stop-actions.json", import.meta.url)));
  for (const pending of ["interrupt", "driver"]) {
    const f = await fixture(t);
    f.gate.current = Promise.withResolvers();
    const { thread } = await f.bridge.request("thread/start", { cwd: f.primary, model: "claude:haiku", wonderPolicy: f.policy, wonderProject: f.project });
    const start = async () => {
      const count = f.inputs.length;
      const { turn } = await f.bridge.request("turn/start", { threadId: thread.id, input: [{ type: "text", text: "Wait" }] });
      await settle(() => f.inputs.length > count);
      return { turn, run: f.bridge.active.get(thread.id) };
    };
    const { turn, run } = await start();
    const held = Promise.withResolvers();
    t.after(() => held.resolve());
    run.query.interrupt = () => held.promise;
    if (pending === "driver") run.cuaDriver = { stop: () => held.promise, close: async () => {} };
    const stop = (body, turnId = turn.id) => f.bridge.request("turn/interrupt", { threadId: thread.id, turnId, ...body });
    for (const body of contract.invalid) await assert.rejects(stop(body), /invalid/);
    assert.equal(run.stopped, false);
    await stop(contract.legacy); // Automatic handoff starts the soft stop, never a tap.
    await stop(contract.first);
    await stop(contract.first);
    await stop(contract.second, "stale-turn");
    assert.equal(f.closes.length, 0);
    await stop(contract.second);
    await run.finished;
    assert.equal(f.sessions.get(thread.id).turns.at(-1).status, "interrupted");
    const closes = f.closes.length;

    // Delayed old Stop requests and a late failure of its soft control must not
    // kill the next run. That run also starts with no inherited owner action.
    f.gate.current = Promise.withResolvers();
    const next = await start();
    held.reject(new Error("old control ended"));
    await run.stopWork;
    await stop(contract.second);
    next.run.query.interrupt = async () => {};
    await stop(contract.second, next.turn.id);
    await stop(contract.second, next.turn.id);
    assert.equal(f.closes.length, closes);
    await stop(contract.first, next.turn.id);
    await next.run.finished;
    assert.equal(f.sessions.get(thread.id).turns.at(-1).status, "interrupted");
    await f.bridge.close();
  }
});

async function interruptedAgent(t) {
  const f = await fixture(t);
  f.lingers.current = true;
  const { thread } = await f.bridge.request("thread/start", { cwd: f.primary, model: "claude:haiku", wonderPolicy: f.policy, wonderProject: f.project });
  f.gate.current = Promise.withResolvers();
  const { turn } = await f.bridge.request("turn/start", { threadId: thread.id, input: [{ type: "text", text: "Run it" }] });
  await settle(() => f.bridge.active.get(thread.id)?.query);
  const run = f.bridge.active.get(thread.id);
  f.emits.push({ type: "system", subtype: "task_started", task_type: "local_agent", task_id: "a0b1c2", tool_use_id: "tu-agent", description: "Audit" });
  await settle(() => run.children.size);
  await f.bridge.request("turn/interrupt", { threadId: thread.id, turnId: turn.id, ownerActionId: "first-owner-stop" });
  await run.finished;
  let transcript = agentTranscript();
  f.bridge.projectMessages = f.bridge.projectTaskMessages = async () => ({ stamp: null, messages: transcript, desktop: false });
  const stopped = [];
  f.bridge.lingering.get(thread.id).query.stopTask = async id => { stopped.push(id); };
  const list = async () => Object.fromEntries((await f.bridge.request("thread/backgroundTasks/list", { threadId: thread.id })).data.map(task => [task.id, task]));
  return { f, thread, stopped, list, setTranscript: value => { transcript = value; } };
}

test("interrupting a project turn spares a running agent and its card", async t => {
  const { f, thread, list } = await interruptedAgent(t);
  assert.equal(f.interrupts.length, 1);
  assert.equal(f.closes.length, 0, "the interrupt must not kill the Claude process");
  assert.equal(f.sessions.get(thread.id).turns.at(-1).status, "interrupted");
  assert.ok(f.bridge.lingering.has(thread.id));
  const agent = (await list())["tu-agent"];
  assert.deepEqual([agent.status, agent.canStop], ["running", true]);
  // The agent's activity card keeps running until the agent's own notice.
  const card = [...f.bridge.lingering.get(thread.id).children.values()][0];
  assert.equal(card.projection.terminal, false);
  assert.equal(f.frames.some(frame => frame.method === "item/completed" && frame.params.item?.type === "subAgentActivity"), false);
  await f.bridge.close();
});

// The native SDK summary describes one live task, not its final report. It must
// survive a spared agent's parent turn without surviving the task or process.
for (const [outcome, expected] of [["completed", "completed"], ["failed", "failed"], ["stopped", "interrupted"]])
test(`native task progress follows exact live ownership and clears when ${outcome}`, async t => {
  const { f, thread, list } = await interruptedAgent(t);
  try {
    const handle = f.bridge.lingering.get(thread.id);
    const emit = async message => {
      const before = handle.roster.tasks;
      f.emits.push({ type: "system", task_id: "a0b1c2", usage: { total_tokens: 30, tool_uses: 2, duration_ms: 30000 }, ...message });
      f.emits.push({ type: "system", subtype: "background_tasks_changed", tasks: [] });
      await settle(() => handle.roster.tasks !== before);
    };
    await emit({ subtype: "task_progress", summary: "  Comparing\nrelease requirements  " });
    assert.equal((await list())["tu-agent"].progressSummary, "Comparing release requirements");
    assert.equal((await list())["tu-agent"].summary, undefined, "final reports are not roster status");
    for (const fields of [{ tool_use_id: "another-call" }, { task_id: "unknown" },
      { session_id: "another-session" }, { ambient: true }, { skip_transcript: true }]) {
      await emit({ subtype: "task_progress", summary: "Wrong task", ...fields });
      assert.equal((await list())["tu-agent"].progressSummary, "Comparing release requirements");
    }
    await emit({ subtype: "task_progress", summary: "Checking " + "x".repeat(5000) });
    assert.equal((await list())["tu-agent"].progressSummary.length, 240);
    await emit({ subtype: "task_progress" });
    assert.equal((await list())["tu-agent"].progressSummary.length, 240, "usage-only progress does not replace a summary");
    await emit({ subtype: "task_progress", summary: " \n\t" });
    assert.equal((await list())["tu-agent"].progressSummary.length, 240, "the SDK skips invalid/blank summaries");
    await emit({ subtype: "task_progress", summary: "🔎".repeat(300) });
    assert.equal([...(await list())["tu-agent"].progressSummary].length, 240, "Unicode targets stay intact");
    await emit({ subtype: "task_progress", summary: "Comparing the final checklist" });
    await emit({ subtype: "task_notification", status: outcome, summary: "Checked the guide." });
    const done = (await list())["tu-agent"];
    assert.deepEqual([done.status, done.progressSummary, done.canStop], [expected, null, false]);
    await emit({ subtype: "task_progress", summary: "Late running copy" });
    assert.equal((await list())["tu-agent"].progressSummary, null);
    const child = [...handle.children.values()][0];
    assert.ok(child.turn.items.some(item => item.type === "agentMessage" && item.text === "Checked the guide."));
    // The existing child completion persists the final report, never running copy.
    const loaded = await new Sessions(f.sessions.root).initialize();
    const saved = (await loaded.load(child.session.id));
    assert.equal(saved.turns[0].status, expected);
    assert.ok(saved.turns[0].items.some(item => item.type === "agentMessage" && item.text === "Checked the guide."));
    assert.ok(!JSON.stringify(saved).includes("Comparing the final checklist"));
  } finally { await f.bridge.close(); }
});

test("Stop clears native task progress even before a task notification arrives", async t => {
  const { f, thread, list } = await interruptedAgent(t);
  try {
    const handle = f.bridge.lingering.get(thread.id);
    f.emits.push({ type: "system", subtype: "task_progress", task_id: "a0b1c2", summary: "Reviewing the guide" });
    f.emits.push({ type: "system", subtype: "background_tasks_changed", tasks: [] });
    await settle(() => handle.roster.tasks !== null);
    assert.equal((await list())["tu-agent"].progressSummary, "Reviewing the guide");
    const stopped = await f.bridge.request("thread/backgroundTask/stop", { threadId: thread.id, taskId: "tu-agent" });
    assert.deepEqual([stopped.outcome, stopped.task.progressSummary], ["stopped", null]);
    assert.equal((await list())["tu-agent"].progressSummary, null);
  } finally { await f.bridge.close(); }
});

test("MCP task summaries use the same live roster and disappear when the task leaves", async t => {
  const { f, thread, list } = await interruptedAgent(t);
  try {
    const handle = f.bridge.lingering.get(thread.id);
    const roster = async tasks => {
      const before = handle.roster.tasks;
      f.emits.push({ type: "system", subtype: "background_tasks_changed", tasks });
      await settle(() => handle.roster.tasks !== before);
    };
    const mcp = { task_id: "mcp-1", task_type: "mcp_task", description: "Check the deployment" };
    await roster([mcp]);
    f.emits.push({ type: "system", subtype: "task_progress", task_id: "mcp-1", summary: "Waiting for the deployment" });
    await roster([mcp]);
    assert.equal((await list())["task:mcp-1"].progressSummary, "Waiting for the deployment");
    await roster([]);
    f.emits.push({ type: "system", subtype: "task_progress", task_id: "mcp-1", summary: "Late progress" });
    await roster([]);
    assert.equal((await list())["task:mcp-1"], undefined);
    // A task that starts again must obtain new provider progress.
    f.emits.push({ type: "system", subtype: "task_started", task_id: "mcp-1", task_type: "mcp_task" });
    await roster([mcp]);
    assert.equal((await list())["task:mcp-1"].progressSummary, null);
    await f.bridge.close();
    assert.equal((await list())["tu-agent"].progressSummary, null, "process loss never rehydrates running copy");
  } finally { await f.bridge.close(); }
});

test("a message after an interrupt joins the same Claude process and the agent stays stoppable", async t => {
  const { f, thread, stopped, list, setTranscript } = await interruptedAgent(t);
  const process = f.bridge.lingering.get(thread.id).query;
  f.gate.current = Promise.withResolvers();
  await f.bridge.request("turn/start", { threadId: thread.id, input: [{ type: "text", text: "Next" }] });
  await settle(() => f.inputs.length === 2);
  assert.equal(f.capturedOptions.length, 1, "no second process");
  assert.equal(f.bridge.active.get(thread.id).query, process);
  assert.equal(f.closes.length, 0);
  assert.equal(f.inputs[1].message.content[0].text, "Next");
  const during = (await list())["tu-agent"];
  assert.deepEqual([during.status, during.canStop], ["running", true]);

  // The reply finishes while the agent still runs: the process stays, and Stop works.
  f.gate.current.resolve();
  await f.bridge.active.get(thread.id)?.finished;
  assert.equal(f.sessions.get(thread.id).turns.at(-1).status, "completed");
  assert.equal(f.closes.length, 0);
  assert.ok(f.bridge.lingering.has(thread.id));
  assert.equal((await f.bridge.request("thread/backgroundTask/stop", { threadId: thread.id, taskId: "tu-agent" })).outcome, "stopped");
  assert.deepEqual(stopped, ["a0b1c2"]);
  assert.equal((await list())["tu-agent"].status, "interrupted");

  // The agent's completion notice, then the process's own result: the
  // process stays warm for the next reply until it has idled too long.
  setTranscript(agentTranscript({ type: "user", uuid: "n1", message: { content:
    "<task-notification>\n<task-id>a0b1c2</task-id>\n<tool-use-id>tu-agent</tool-use-id>\n<status>completed</status>\n<summary>Agent \"Audit\" done</summary>\n</task-notification>" } }));
  assert.equal((await list())["tu-agent"].status, "completed");
  f.emits.push({ type: "result", subtype: "success" });
  await new Promise(resolve => setTimeout(resolve, 100));
  assert.equal(f.closes.length, 0);
  assert.ok(f.bridge.lingering.has(thread.id));
  f.bridge.warmIdleMs = 0;
  await settle(() => f.closes.length);
  assert.equal(f.closes.length, 1);
  assert.ok(!f.bridge.lingering.has(thread.id));
});

// Model and effort changes use native session controls before the next input.
// Background work keeps the same process and its own Stop action.
test("queued model and effort apply before input without stopping background work", async t => {
  const { f, thread, list } = await interruptedAgent(t);
  const handle = f.bridge.lingering.get(thread.id);
  const apply = handle.query.applyFlagSettings;
  const control = Promise.withResolvers();
  let requested;
  handle.query.applyFlagSettings = async settings => { requested = settings; await control.promise; await apply(settings); };
  f.gate.current = null;
  const sent = await f.bridge.request("turn/start", { threadId: thread.id, clientUserMessageId: "11111111-1111-4111-8111-111111111111",
    model: "claude:sonnet", effort: "high", input: [{ type: "text", text: "Edited queued message" }] });
  assert.equal(sent.deferredSettings, undefined, "the selected model must not silently become the old one");
  await settle(() => requested);
  assert.equal(f.inputs.length, 1, "the message waits for native settings confirmation");
  assert.equal(requested.model, "sonnet");
  assert.equal(requested.effortLevel, "high");
  assert.deepEqual(requested.availableModels, ["sonnet"]);
  control.resolve();
  await f.bridge.active.get(thread.id)?.finished;
  assert.equal(f.inputs.at(-1).message.content[0].text, "Edited queued message");
  assert.equal(f.inputSettings.at(-1).model, "sonnet");
  assert.equal(f.inputSettings.at(-1).effortLevel, "high");
  assert.equal(f.capturedOptions.length, 1);
  assert.equal(f.closes.length, 0);
  assert.deepEqual([(await list())["tu-agent"].status, (await list())["tu-agent"].canStop], ["running", true]);
  // A later message can clear effort and switch models again in delivery order.
  handle.query.applyFlagSettings = apply;
  await f.bridge.request("turn/start", { threadId: thread.id, model: "claude:haiku", effort: null,
    input: [{ type: "text", text: "Next queued message" }] });
  await f.bridge.active.get(thread.id)?.finished;
  assert.equal(f.inputSettings.at(-1).model, "haiku");
  assert.equal(f.inputSettings.at(-1).effortLevel, null);
  assert.equal(f.capturedOptions.length, 1);
});

test("unconfirmed native settings never send a queued message with the old model", async t => {
  const { f, thread, list } = await interruptedAgent(t);
  const handle = f.bridge.lingering.get(thread.id);
  const apply = handle.query.applyFlagSettings;
  handle.query.applyFlagSettings = async () => { throw new Error("Native model change refused"); };
  f.gate.current = null;
  const { turn } = await f.bridge.request("turn/start", { threadId: thread.id, model: "claude:sonnet", effort: "high",
    input: [{ type: "text", text: "Use the selected model" }] });
  await f.bridge.active.get(thread.id)?.finished;
  assert.equal(f.inputs.length, 1);
  assert.equal(turn.status, "failed");
  assert.match(turn.error.message, /Native model change refused/);
  assert.equal(f.closes.length, 0);
  assert.equal((await list())["tu-agent"].status, "running");
  handle.query.applyFlagSettings = apply;
  await f.bridge.request("turn/start", { threadId: thread.id, model: "claude:sonnet", effort: "high",
    input: [{ type: "text", text: "Try the model again" }] });
  await f.bridge.active.get(thread.id)?.finished;
  assert.equal(f.inputs.length, 2);
  assert.equal(f.inputSettings.at(-1).model, "sonnet");
});

test("Stop during a native model change prevents the waiting input", async t => {
  const { f, thread, list } = await interruptedAgent(t);
  const handle = f.bridge.lingering.get(thread.id);
  const apply = handle.query.applyFlagSettings;
  const control = Promise.withResolvers();
  let requested = false;
  handle.query.applyFlagSettings = async settings => { requested = true; await control.promise; await apply(settings); };
  f.gate.current = null;
  const { turn } = await f.bridge.request("turn/start", { threadId: thread.id, model: "claude:sonnet", effort: "high",
    input: [{ type: "text", text: "Stop before delivery" }] });
  await settle(() => requested);
  await f.bridge.request("turn/interrupt", { threadId: thread.id, turnId: turn.id });
  control.resolve();
  await f.bridge.active.get(thread.id)?.finished;
  assert.equal(turn.status, "interrupted");
  assert.equal(f.inputs.length, 1);
  assert.equal(f.closes.length, 0);
  assert.equal((await list())["tu-agent"].status, "running");
});

test("a late native model acknowledgement cannot overtake the next queued selection", async t => {
  const { f, thread, list } = await interruptedAgent(t);
  const handle = f.bridge.lingering.get(thread.id);
  const apply = handle.query.applyFlagSettings;
  const control = Promise.withResolvers();
  let calls = 0;
  handle.query.applyFlagSettings = async settings => { calls++; await control.promise; await apply(settings); };
  f.gate.current = null;
  const { turn } = await f.bridge.request("turn/start", { threadId: thread.id, model: "claude:sonnet", effort: "high",
    input: [{ type: "text", text: "Wait for model confirmation" }] });
  await f.bridge.active.get(thread.id)?.finished;
  assert.equal(turn.status, "failed");
  assert.match(turn.error.message, /did not confirm the model change/);
  assert.equal(f.inputs.length, 1);
  assert.equal(calls, 1);
  assert.equal((await list())["tu-agent"].status, "running");
  assert.equal(f.closes.length, 0);
  await f.bridge.request("turn/start", { threadId: thread.id, model: "claude:opus", effort: "max",
    input: [{ type: "text", text: "Use Opus instead" }] });
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(calls, 1, "a pending SDK control stays owned until it settles");
  assert.equal(f.inputs.length, 1);
  control.resolve();
  await f.bridge.active.get(thread.id)?.finished;
  assert.equal(calls, 2);
  assert.equal(f.inputs.length, 2);
  assert.equal(f.inputSettings.at(-1).model, "opus");
  assert.equal(f.inputSettings.at(-1).effortLevel, "max");
  assert.equal(f.capturedOptions.length, 1);
});

// Folders, tools and the sandbox are not deferred: the reply would run with
// access the owner just took away.
test("a folder or access change while background work runs is still refused with its reason", async t => {
  const { f, thread } = await interruptedAgent(t);
  f.gate.current = null;
  await assert.rejects(f.bridge.request("turn/start", { threadId: thread.id, model: "claude:haiku",
    wonderPolicy: { ...f.policy, mode: "read_only" }, input: [{ type: "text", text: "Read only now" }] }), /still running background tasks/);
  assert.equal(f.capturedOptions.length, 1);

});

// Owner: the agent-task list counts what Claude on the Mac counts as running:
// agents, background commands, monitors and linked sessions it started.
test("monitors are tasks; linked sessions and proposals are not", () => {
  const messages = [
    { type: "user", uuid: "turn-1", origin: { kind: "human" }, message: { content: "Watch the build" } },
    { type: "assistant", uuid: "a1", timestamp: "2026-10-09T00:00:01Z", message: { content: [
      { type: "tool_use", id: "mon", name: "Monitor", input: { description: "Build errors", command: "tail -f build.log" } },
      { type: "tool_use", id: "linked", name: "mcp__ccd_session__start_session", input: { title: "Review", prompt: "Review it" } },
      { type: "tool_use", id: "offer", name: "mcp__ccd_session__start_session", input: { title: "Maybe", prompt: "Later" } }] } },
    { type: "user", uuid: "r1", message: { content: [
      { type: "tool_result", tool_use_id: "mon", content: "Monitor started (task b1a2c3d4e, expires in 30m unless the source ends first)" },
      { type: "tool_result", tool_use_id: "linked", content: [{ type: "text", text: "Started session \"Review\" (session_id: local_1626a88b-15e9, name: review). It runs on its own." }] },
      { type: "tool_result", tool_use_id: "offer", content: [{ type: "text", text: "Proposed \"Maybe\" (task_id: task_ab07e02f)." }] }] } },
  ];
  const tasks = Object.fromEntries(backgroundTasks(messages, true).map(t => [t.id, t]));
  assert.deepEqual(Object.keys(tasks).sort(), ["mon"], "Claude leaves spawned sessions out of Background tasks");
  assert.deepEqual([tasks.mon.kind, tasks.mon.status, tasks.mon.taskId, tasks.mon.title], ["monitor", "running", "b1a2c3d4e", "Build errors"]);
  // Without a live process a monitor's state is unknown.
  const idle = Object.fromEntries(backgroundTasks(messages, false).map(t => [t.id, t.status]));
  assert.deepEqual(idle, { mon: "unknown" });
  const done = [...messages, { type: "user", uuid: "n1", origin: { kind: "task-notification" }, message: { content:
    "<task-notification>\n<task-id>b1a2c3d4e</task-id>\n<tool-use-id>mon</tool-use-id>\n<status>completed</status>\n<summary>Monitor \"Build errors\" ended</summary>\n</task-notification>" } }];
  assert.equal(backgroundTasks(done, true).find(t => t.id === "mon").status, "completed");
});

// The SDK exposes these native flags on messages returned after compaction.
// They must not become owner turns or change a completed main reply to running.
test("native compact-summary and meta flags never become owner prompts", () => {
  const prompt = { type: "user", uuid: "u", message: { content: "Go" } };
  const call = { type: "assistant", uuid: "a", message: { id: "a", content: [{ type: "tool_use", id: "t", name: "Bash", input: { command: "true" } }] } };
  const result = { type: "user", uuid: "r", is_meta: true, message: { content: [{ type: "tool_result", tool_use_id: "t", content: "ok" }] } };
  const reply = { type: "assistant", uuid: "b", message: { stop_reason: "end_turn", content: [{ type: "text", text: "Done" }] } };
  for (const flag of ["isCompactSummary", "is_meta", "isMeta"]) {
    const system = { type: "user", uuid: "summary", [flag]: true, message: { content: "Private harness context" } };
    const messages = [prompt, call, result, reply, system];
    assert.deepEqual(nativeTurns(messages).map(turn => turn.id), ["u"], flag);
    assert.equal(nativeTurns(messages)[0].items[1].status, "completed", "meta tool results still settle their actual tool");
    assert.equal(mainTurnRunning(messages), false, flag);
    assert.equal(turnEndMessageId(messages, "summary"), null, flag);
  }
});

test("a main turn ends at its final reply even while background work runs", () => {
  const prompt = { type: "user", uuid: "u", message: { content: "Go" } };
  const tool = { type: "assistant", uuid: "a", message: { stop_reason: "tool_use", content: [{ type: "tool_use", id: "t", name: "Bash", input: {} }] } };
  const result = { type: "user", uuid: "r", message: { content: [{ type: "tool_result", tool_use_id: "t", content: "ok" }] } };
  const reply = { type: "assistant", uuid: "b", message: { stop_reason: "end_turn", content: [{ type: "text", text: "Started" }] } };
  const child = { type: "assistant", uuid: "c", parent_tool_use_id: "t", message: { stop_reason: null, content: [] } };
  assert.equal(mainTurnRunning([prompt]), true);
  assert.equal(mainTurnRunning([prompt, tool]), true);
  assert.equal(mainTurnRunning([prompt, tool, result]), true);
  assert.equal(mainTurnRunning([prompt, tool, result, reply]), false);
  assert.equal(mainTurnRunning([prompt, tool, result, reply, child]), false, "an agent's own messages are not the main turn");
  assert.equal(mainTurnRunning([prompt, { type: "user", uuid: "i", message: { content: [{ type: "text", text: "[Request interrupted by user]" }] } }]), false);
  assert.equal(mainTurnRunning([]), false);
});

test("a desktop session's status separates its reply from background work", async t => {
  const dir = await mkdtemp(join(tmpdir(), "wonder-claude-sessions-"));
  t.after(() => rm(dir, { recursive: true, force: true }));
  const record = status => writeFile(join(dir, `${process.pid}.json`), JSON.stringify({ pid: process.pid, procStart: processLabel(process.pid), sessionId: "s", status }));
  const state = async status => { await record(status); return claudeSessionState("s", dir); };
  assert.deepEqual(await state("busy"), { open: true, busy: true, background: true, busySince: null, startedAt: null });
  assert.deepEqual(await state("shell"), { open: true, busy: false, background: true, busySince: null, startedAt: null }, "only background commands or monitors");
  assert.deepEqual(await state("waiting"), { open: true, busy: true, background: true, busySince: null, startedAt: null });
  assert.deepEqual(await state("idle"), { open: true, busy: false, background: false, busySince: null, startedAt: null });
  for (const status of [undefined, null, "future-state"]) {
    const unknown = await state(status);
    assert.equal(unknown.busy, null, "unrecognized native status is unknown");
    assert.equal(await claudeSessionBusy("s", dir), null, "unknown is never relabeled as idle or busy");
  }
  await writeFile(join(dir, `${process.pid}.json`), JSON.stringify({ pid: process.pid, procStart: processLabel(process.pid), sessionId: "s", status: "busy", statusUpdatedAt: 1234 }));
  assert.equal((await claudeSessionState("s", dir)).busySince, 1234, "when its reply started");
  // Another Agent SDK app's process (this test's own, which the runtime did
  // not start) is a writer; the runtime's own children are not.
  await writeFile(join(dir, `${process.pid}.json`), JSON.stringify({ pid: process.pid, procStart: processLabel(process.pid), sessionId: "s", status: "busy", entrypoint: "sdk-ts" }));
  assert.deepEqual(await claudeSessionState("s", dir), { open: true, busy: true, background: true, busySince: null, startedAt: null }, "another SDK app holds the chat");
  const own = spawn(process.execPath, ["-e", "setTimeout(() => {}, 30000)"]);
  t.after(() => own.kill());
  await rm(join(dir, `${process.pid}.json`));
  await writeFile(join(dir, `${own.pid}.json`), JSON.stringify({ pid: own.pid, sessionId: "s", status: "busy", entrypoint: "sdk-ts" }));
  assert.deepEqual(await claudeSessionState("s", dir), { open: false, busy: false, background: false, busySince: null, startedAt: null }, "Cupola's own SDK runs are not another writer");
});

test("a desktop session whose reply finished is idle while its tasks run, and refuses a second writer", async t => {
  const f = await fixture(t, { transcripts: {} });
  const dir = await mkdtemp(join(tmpdir(), "wonder-claude-sessions-"));
  t.after(() => rm(dir, { recursive: true, force: true }));
  const sdkSessionId = "11111111-2222-3333-4444-555555555556";
  const messages = [{ type: "user", uuid: "turn-1", origin: { kind: "human" }, message: { content: "Audit it" } },
    { type: "assistant", uuid: "a1", message: { id: "m1", stop_reason: "tool_use", content: [{ type: "tool_use", id: "tu-a", name: "Agent", input: { description: "Audit", prompt: "Look" } }] } },
    { type: "user", uuid: "r1", message: { content: [{ type: "tool_result", tool_use_id: "tu-a", content: "Async agent launched successfully. agentId: x9" }] } },
    { type: "assistant", uuid: "a2", message: { id: "m2", stop_reason: "end_turn", content: [{ type: "text", text: "It is running." }] } }];
  const sdk = { getSessionMessages: async () => messages, getSessionInfo: async (id, { dir: cwd }) => ({ sessionId: id, cwd, lastModified: 1 }) };
  const bridge = new ClaudeBridge({ updates: { acquire: async () => ({ runtime: { sdk }, release: async () => {} }) },
    sessions: f.sessions, send: async () => {}, claudeSessionsDir: dir });
  syntheticTaskFiles(bridge, () => messages);
  t.after(() => bridge.close());
  const { thread } = await bridge.request("project/session/attach", { sessionId: sdkSessionId, model: "claude:haiku", wonderPolicy: f.policy, wonderProject: f.project });
  await writeFile(join(dir, `${process.pid}.json`), JSON.stringify({ pid: process.pid, procStart: processLabel(process.pid), sessionId: sdkSessionId, status: "busy" }));
  const { data } = await bridge.request("thread/turns/list", { threadId: thread.id, itemsView: "notLoaded" });
  assert.equal(data[0].status, "completed");
  assert.equal(data[0].runningElsewhere, undefined);
  const tasks = (await bridge.request("thread/backgroundTasks/list", { threadId: thread.id })).data;
  assert.deepEqual(tasks.map(t => [t.id, t.status, t.runningElsewhere]), [["tu-a", "running", true]]);
  // Claude Code 2.1.295 reports "idle" while a background command runs: an
  // open process still runs what its transcript launched and never reported.
  await writeFile(join(dir, `${process.pid}.json`), JSON.stringify({ pid: process.pid, procStart: processLabel(process.pid), sessionId: sdkSessionId, status: "idle" }));
  bridge.transcripts.clear();
  const idle = (await bridge.request("thread/backgroundTasks/list", { threadId: thread.id })).data;
  assert.deepEqual(idle.map(t => [t.id, t.status, t.runningElsewhere]), [["tu-a", "running", true]]);
  // Lists ask only for tasks running elsewhere: an open process still needs
  // the transcript, a chat no process on the Mac holds does not.
  let reads = 0; const read = sdk.getSessionMessages;
  sdk.getSessionMessages = async (...args) => { reads += 1; return read(...args); };
  const elsewhere = async () => (await bridge.request("thread/backgroundTasks/list", { threadId: thread.id, runningElsewhereOnly: true })).data;
  bridge.transcripts.clear();
  assert.deepEqual((await elsewhere()).map(t => [t.id, t.runningElsewhere]), [["tu-a", true]]);
  assert.equal(reads, 0);
  await rm(join(dir, `${process.pid}.json`));
  bridge.transcripts.clear();
  assert.deepEqual(await elsewhere(), [], "nothing on the Mac holds the chat");
  assert.equal(reads, 0, "no SDK transcript read for it");
  const closed = (await bridge.request("thread/backgroundTasks/list", { threadId: thread.id })).data;
  assert.ok(closed.every(t => !t.runningElsewhere), "the full list agrees");
  await writeFile(join(dir, `${process.pid}.json`), JSON.stringify({ pid: process.pid, procStart: processLabel(process.pid), sessionId: sdkSessionId, status: "idle" }));
  // Cupola's host ends the app's process before sending; a process still (or
  // again) open here would write its own copy, so the bridge refuses a second writer.
  await assert.rejects(bridge.request("turn/start", { threadId: thread.id, input: [{ type: "text", text: "Another" }] }), /open in Claude on your computer/);
});

// Claude's Background tasks panel lists every Bash run, foreground ones too,
// with how long each ran; Cupola's list and count match it.
test("foreground Bash runs are part of the reply, not tasks; one moved to the background is", async t => {
  const at = s => `2026-10-10T10:00:${String(s).padStart(2, "0")}Z`;
  const use = (id, command, extra = {}) => ({ type: "tool_use", id, name: "Bash", input: { command, ...extra } });
  const result = (id, s, content, is_error) => ({ type: "user", uuid: `r-${id}`, timestamp: at(s), message: { content: [{ type: "tool_result", tool_use_id: id, content, ...(is_error ? { is_error } : {}) }] } });
  const call = (id, s, block) => ({ type: "assistant", uuid: `a-${id}`, timestamp: at(s), message: { id: `m-${id}`, stop_reason: "tool_use", content: [block] } });
  const messages = [
    { type: "user", uuid: "turn-1", origin: { kind: "human" }, message: { content: "Check it" } },
    call("ok", 0, use("ok", "git status", { description: "Show working tree status" })), result("ok", 9, "clean"),
    call("bad", 10, use("bad", "make lint\nmake test")), result("bad", 12, "Exit code 2", true),
    call("long", 21, use("long", "npm test", { description: "Run tests", run_in_background: true })), result("long", 21, "Command running in background with ID: bl1. Output is being written to: /tmp/x"),
    call("slow", 40, use("slow", "xcodebuild", { description: "Build the app" })), result("slow", 40, "Command did not complete within its 600s timeout and was moved to the background (ID: bs2). Output is being written to: /tmp/y"),
    call("read", 41, { type: "tool_use", id: "read", name: "Read", input: { file_path: "/p/a" } }), result("read", 42, "text"),
    call("now", 43, use("now", "swift build", { description: "Build" })),
  ];
  // Claude's Background tasks counts what Claude Code gave a task ID.
  const tasks = backgroundTasks(messages, true);
  assert.deepEqual(tasks.map(t => [t.id, t.kind, t.taskId, t.status]), [["slow", "command", "bs2", "running"], ["long", "command", "bl1", "running"]]);

  // The bridge lists the same rows.
  const f = await fixture(t);
  const sdk = { getSessionMessages: async () => messages, getSessionInfo: async (id, { dir: cwd }) => ({ sessionId: id, cwd, lastModified: 1 }) };
  const dir = await mkdtemp(join(tmpdir(), "wonder-claude-sessions-"));
  t.after(() => rm(dir, { recursive: true, force: true }));
  const bridge = new ClaudeBridge({ updates: { acquire: async () => ({ runtime: { sdk }, release: async () => {} }) },
    sessions: f.sessions, send: async () => {}, claudeSessionsDir: dir });
  syntheticTaskFiles(bridge, () => messages);
  t.after(() => bridge.close());
  const { thread } = await bridge.request("project/session/attach", { sessionId: "11111111-2222-3333-4444-555555555557", model: "claude:haiku", wonderPolicy: f.policy, wonderProject: f.project });
  const listed = (await bridge.request("thread/backgroundTasks/list", { threadId: thread.id })).data;
  assert.deepEqual(listed.map(task => task.id), ["slow", "long"]);
  assert.ok(listed.every(task => !("result" in task) && !("request" in task) && !("summary" in task)));
});

test("the live roster names running tasks the transcript has not recorded", () => {
  const roster = { tasks: new Map([["b1", { task_id: "b1", task_type: "local_bash", description: "Sleep" }],
    ["m1", { task_id: "m1", task_type: "monitor_mcp", description: "Watch" }]]), toolUses: new Map([["b1", "tu-b"]]) };
  const transcript = [{ id: "tu-b", kind: "command", status: "unknown", taskId: null, title: "Sleep" },
    { id: "tu-old", kind: "command", status: "completed", taskId: "b0", title: "Old" }];
  const merged = withLiveRoster(transcript, roster);
  assert.deepEqual(merged.map(t => [t.id, t.kind, t.status, t.taskId]),
    [["task:m1", "monitor", "running", "m1"], ["tu-b", "command", "running", "b1"], ["tu-old", "command", "completed", "b0"]]);
  assert.equal(withLiveRoster(transcript, { tasks: null, toolUses: new Map() }), transcript, "no roster yet: the transcript decides");
  const finished = withLiveRoster(transcript, { tasks: new Map(), toolUses: new Map(),
    updates: new Map([["b0", { status: "unknown", progressSummary: null }]]) });
  assert.equal(finished.find(t => t.id === "tu-old").status, "completed", "roster removal must not replace a later transcript outcome");
});

// SDK process lifecycle is distinct from the prompt receipt: a continuation
// can be working after the original reply completed, without reopening it.
test("native session state protects a retained continuation and preserves its completed receipt", async t => {
  const f = await fixture(t);
  f.lingers.current = true;
  const { thread } = await f.bridge.request("thread/start", { cwd: f.primary, model: "claude:haiku", wonderPolicy: f.policy, wonderProject: f.project });
  const { turn } = await f.bridge.request("turn/start", { threadId: thread.id, input: [{ type: "text", text: "Work" }] });
  await f.bridge.active.get(thread.id).finished;
  const session = f.sessions.get(thread.id);
  assert.equal((await f.bridge.request("thread/read", { threadId: thread.id })).thread.status.type, "notLoaded", "missing native process events stay unknown");
  const state = value => ({ type: "system", subtype: "session_state_changed", state: value, session_id: session.sdkSessionId, uuid: `state-${value}` });
  f.emits.push(state("running"));
  f.emits.push({ type: "assistant", message: { id: "continued", content: [{ type: "text", text: "Continuation begins" }] } });
  await settle(() => f.frames.some(frame => frame.params?.item?.text === "Continuation begins"));
  assert.equal((await f.bridge.request("thread/read", { threadId: thread.id })).thread.status.type, "active");
  assert.deepEqual((await f.bridge.request("thread/loaded/list", {})).data, [thread.id]);
  // A visible native session is not lost behind retained sessions or helper
  // threads, and this batch only describes existing processes.
  const unrelated = Array.from({ length: 201 }, (_, i) => [`unrelated-${i}`, { ...session, id: `unrelated-${i}`, sdkSessionId: `other-${i}` }]);
  const child = { ...session, id: "helper", parent: { threadId: session.id, depth: 1, name: "Helper" } };
  f.sessions.values = new Map([...unrelated, [child.id, child], [session.id, session]]);
  const listed = await f.bridge.request("thread/list", { sessionIds: [session.sdkSessionId], limit: 200 });
  assert.equal(listed.nextCursor, null);
  assert.deepEqual(listed.data.map(row => [row.id, row.sessionId, row.status.type]), [[thread.id, session.sdkSessionId, "active"]]);
  assert.equal(f.capturedOptions.length, 1, "Listing cannot create or resume a Query");
  f.sessions.values = new Map([[session.id, session]]);
  assert.equal(session.turns.at(-1).status, "completed", "process state never rewrites the durable receipt");
  await assert.rejects(f.bridge.request("turn/start", { threadId: thread.id, input: [{ type: "text", text: "Next" }] }), /already has an active turn/);
  await assert.rejects(f.bridge.request("project/session/fork", { threadId: thread.id }), /Wait for Claude to finish/);
  await assert.rejects(f.bridge.request("thread/settings/update", { threadId: thread.id, model: "claude:sonnet" }), /Wait for Claude to finish/);
  const handle = f.bridge.lingering.get(thread.id);
  f.bridge.warmIdleMs = 0; f.bridge.maxWarm = 1;
  await f.bridge.evictWarm("another-thread");
  assert.equal(f.closes.length, 0, "native working survives warm-process eviction");
  const ask = f.capturedOptions[0].canUseTool("AskUserQuestion", { questions: [{ question: "Which option?", header: "Choice", options: [
    { label: "One", description: "First" }, { label: "Two", description: "Second" }], multiSelect: false }] }, { toolUseID: "native-question" });
  await settle(() => f.frames.some(frame => frame.method === "item/tool/requestUserInput"));
  f.emits.push(state("requires_action"));
  await settle(() => handle.sessionState === "requires_action");
  assert.deepEqual((await f.bridge.request("thread/read", { threadId: thread.id })).thread.status,
    { type: "active", activeFlags: ["waitingOnUserInput"] });
  const request = f.frames.find(frame => frame.method === "item/tool/requestUserInput");
  await f.bridge.receive({ id: request.id, result: { answers: { [request.params.questions[0].id]: { answers: ["One"] } } } });
  await ask;
  f.bridge.warmIdleMs = 60_000;
  f.emits.push(state("idle"));
  await settle(() => f.frames.some(frame => frame.method === "thread/status/changed" && frame.params.status.type === "idle"));
  assert.equal((await f.bridge.request("thread/read", { threadId: thread.id })).thread.status.type, "idle");
  assert.deepEqual((await f.bridge.request("thread/loaded/list", {})).data, []);
  assert.equal(f.frames.filter(frame => frame.method === "turn/completed" && frame.params.turnId === turn.id).length, 1);
});

test("native idle cannot complete an unanswered prompt and stale process events cannot revive it", async t => {
  const f = await fixture(t);
  f.lingers.current = true;
  f.gate.current = Promise.withResolvers();
  const { thread } = await f.bridge.request("thread/start", { cwd: f.primary, model: "claude:haiku", wonderPolicy: f.policy, wonderProject: f.project });
  const { turn } = await f.bridge.request("turn/start", { threadId: thread.id, input: [{ type: "text", text: "Work" }] });
  await settle(() => f.bridge.active.get(thread.id)?.handle);
  const run = f.bridge.active.get(thread.id), session = f.sessions.get(thread.id), handle = run.handle;
  const state = (value, sessionId = session.sdkSessionId) => ({ type: "system", subtype: "session_state_changed", state: value, session_id: sessionId });
  f.emits.push(state("idle"));
  await settle(() => handle.sessionState === "idle");
  assert.equal((await f.bridge.request("thread/read", { threadId: thread.id })).thread.status.type, "active", "unanswered accepted prompt still owns admission");
  assert.equal(session.turns.at(-1).status, "inProgress");
  assert.ok(!f.frames.some(frame => frame.method === "turn/completed"));
  f.gate.current.resolve();
  await run.finished;
  assert.equal((await f.bridge.request("thread/read", { threadId: thread.id })).thread.status.type, "idle");
  f.emits.push(state("running", "another-session"));
  f.emits.push({ ...state("requires_action"), parent_tool_use_id: "child" });
  f.emits.push(state("future-state"));
  f.emits.push({ type: "assistant", message: { id: "barrier", content: [{ type: "text", text: "Event barrier" }] } });
  await settle(() => f.frames.some(frame => frame.params?.item?.text === "Event barrier"));
  assert.equal((await f.bridge.request("thread/read", { threadId: thread.id })).thread.status.type, "idle", "foreign and unsupported events are ignored");
  await f.bridge.endLinger(thread.id);
  await f.bridge.observeSessionState(session, handle, state("running"));
  assert.equal((await f.bridge.request("thread/read", { threadId: thread.id })).thread.status.type, "idle", "released process cannot revive the chat");
  assert.equal(session.turns.find(t => t.id === turn.id).status, "completed");
});

test("a reply ends at its result while a background command runs; the next message joins the process", async t => {
  const f = await fixture(t);
  f.lingers.current = true;
  const { thread } = await f.bridge.request("thread/start", { cwd: f.primary, model: "claude:haiku", wonderPolicy: f.policy, wonderProject: f.project });
  f.gate.current = Promise.withResolvers();
  await f.bridge.request("turn/start", { threadId: thread.id, input: [{ type: "text", text: "Start a build" }] });
  await settle(() => f.bridge.active.get(thread.id)?.query);
  const run = f.bridge.active.get(thread.id);
  f.emits.push({ type: "system", subtype: "task_started", task_type: "local_bash", task_id: "bq77", tool_use_id: "tu-bg", description: "Build" });
  f.emits.push({ type: "system", subtype: "background_tasks_changed", tasks: [{ task_id: "bq77", task_type: "local_bash", description: "Build" }] });
  await settle(() => run.roster.tasks?.size);
  f.bridge.projectMessages = f.bridge.projectTaskMessages = async () => ({ stamp: null, messages: [], desktop: false });
  f.gate.current.resolve();
  await run.finished;
  assert.equal(f.sessions.get(thread.id).turns.at(-1).status, "completed", "the reply is done");
  assert.ok(!f.bridge.active.has(thread.id), "the conversation is idle");
  assert.equal(f.closes.length, 0, "the process stays up for the command");
  assert.ok(f.bridge.lingering.has(thread.id));
  // Reconnect asks which sessions keep a process, so it never ends this one.
  assert.deepEqual((await f.bridge.request("thread/loaded/list", {})).data, [thread.id]);
  // A list asks about its own threads only and never reads another chat's tasks.
  const judged = [], judge = f.bridge.lingerHasTasks;
  f.bridge.lingerHasTasks = function (session, handle) { judged.push(session.id); return judge.call(this, session, handle); };
  assert.deepEqual((await f.bridge.request("thread/loaded/list", { threadIds: ["another"] })).data, []);
  assert.deepEqual(judged, []);
  assert.deepEqual((await f.bridge.request("thread/loaded/list", { threadIds: [thread.id] })).data, [thread.id]);
  await assert.rejects(f.bridge.request("thread/loaded/list", { threadIds: "all" }), /Choose fewer/);
  f.bridge.lingerHasTasks = judge;
  const listed = (await f.bridge.request("thread/backgroundTasks/list", { threadId: thread.id })).data;
  assert.deepEqual(listed.map(t => [t.id, t.kind, t.status]), [["tu-bg", "command", "running"]]);
  const summary = (await f.bridge.request("thread/backgroundTasks/list", { threadId: thread.id, runningOnly: true })).data;
  assert.deepEqual(summary.map(t => [t.id, t.status, t.runningElsewhere]), [["tu-bg", "running", false]], "lists count Cupola's retained work too");


  f.gate.current = null;
  await f.bridge.request("turn/start", { threadId: thread.id, input: [{ type: "text", text: "Next" }] });
  await f.bridge.active.get(thread.id)?.finished;
  assert.equal(f.capturedOptions.length, 1, "no second process");
  assert.equal(f.inputs.at(-1).message.content[0].text, "Next");
  assert.equal(f.sessions.get(thread.id).turns.at(-1).status, "completed");
  assert.ok(f.bridge.lingering.has(thread.id), "still up while the command runs");

  // The command finishes; Claude reports it in its own turn. That reply joins
  // the last turn live, under the IDs history gives it, and the turn is not
  // reported as ending again. The process then stays warm until idle.
  const last = f.sessions.get(thread.id).turns.at(-1).id;
  const before = f.frames.length;
  f.emits.push({ type: "system", subtype: "background_tasks_changed", tasks: [] });
  f.emits.push({ type: "user", uuid: "wake-bq77", origin: { kind: "task-notification" }, message: { role: "user", content: "<task-notification><task-id>bq77</task-id><status>completed</status><summary>Background command \"Build\" completed (exit code 0)</summary></task-notification>" } });
  f.emits.push({ type: "stream_event", event: { type: "message_start", message: { id: "auto" } } });
  f.emits.push({ type: "stream_event", event: { type: "content_block_start", index: 0, content_block: { type: "text", text: "" } } });
  f.emits.push({ type: "stream_event", event: { type: "content_block_delta", index: 0, delta: { type: "text_delta", text: "The build " } } });
  f.emits.push({ type: "assistant", message: { id: "auto", content: [{ type: "text", text: "The build finished." }] } });
  f.emits.push({ type: "result", subtype: "success" });
  await settle(() => f.frames.slice(before).some(frame => frame.method === "item/completed"));
  f.bridge.warmIdleMs = 0;
  await settle(() => f.closes.length);
  assert.ok(!f.bridge.lingering.has(thread.id));
  assert.deepEqual((await f.bridge.request("thread/loaded/list", {})).data, []);
  const after = f.frames.slice(before);
  assert.ok(!after.some(frame => ["turn/started", "turn/completed"].includes(frame.method)), "the turn stays ended");
  const done = after.filter(frame => frame.method === "item/completed").map(frame => frame.params);
  assert.deepEqual(done.map(p => [p.turnId, p.item.id, p.item.text]), [["wake-bq77", "wake-bq77", "Command \"Build\" finished"],
    ["wake-bq77", "wake-bq77:auto:0", "The build finished."]], "the report and its reply are a run of their own");
  const history = nativeTurns([{ type: "user", uuid: last, message: { content: "Next" } },
    { type: "assistant", message: { id: "done", stop_reason: "end_turn", content: [{ type: "text", text: "Started." }] } },
    { type: "user", uuid: "wake-bq77", origin: { kind: "task-notification" }, message: { content: "<task-notification><task-id>bq77</task-id><status>completed</status><summary>Background command \"Build\" completed (exit code 0)</summary></task-notification>" } },
    { type: "assistant", message: { id: "auto", content: [{ type: "text", text: "The build finished." }] } }]);
  assert.deepEqual(history.at(-1).items.map(item => item.id), done.map(p => p.item.id), "history shows the same items");
  assert.equal(history.length, 2);
});

test("a lingering process yields when Claude on the Mac opens the chat, so one process writes it", async t => {
  const dir = await mkdtemp(join(tmpdir(), "wonder-claude-sessions-"));
  t.after(() => rm(dir, { recursive: true, force: true }));
  const f = await fixture(t, { claudeSessionsDir: dir });
  f.lingers.current = true;
  const { thread } = await f.bridge.request("thread/start", { cwd: f.primary, model: "claude:haiku", wonderPolicy: f.policy, wonderProject: f.project });
  f.gate.current = Promise.withResolvers();
  await f.bridge.request("turn/start", { threadId: thread.id, input: [{ type: "text", text: "Start a build" }] });
  await settle(() => f.bridge.active.get(thread.id)?.query);
  const run = f.bridge.active.get(thread.id);
  f.emits.push({ type: "system", subtype: "background_tasks_changed", tasks: [{ task_id: "bq77", task_type: "local_bash", description: "Build" }] });
  await settle(() => run.roster.tasks?.size);
  f.bridge.projectMessages = f.bridge.projectTaskMessages = async () => ({ stamp: null, messages: [], desktop: false });
  f.gate.current.resolve();
  await run.finished;
  assert.ok(f.bridge.lingering.has(thread.id), "the process stays up for the command");
  await writeFile(join(dir, `${process.pid}.json`), "{");
  assert.equal(await f.bridge.openOnMac(f.sessions.get(thread.id)), false, "unknown is not proof that another app took over");
  await new Promise(resolve => setTimeout(resolve, 60));
  assert.ok(f.bridge.lingering.has(thread.id), "incomplete external evidence never closes Cupola's owned background work");
  await writeFile(join(dir, `${process.pid}.json`), JSON.stringify({ pid: process.pid, procStart: processLabel(process.pid), sessionId: f.sessions.get(thread.id).sdkSessionId, status: "idle", entrypoint: "claude-desktop" }));
  await settle(() => f.closes.length);
  assert.ok(!f.bridge.lingering.has(thread.id), "Cupola released its process");
});

test("a reply does not keep its process for background work when Claude on the Mac has the chat open", async t => {
  const dir = await mkdtemp(join(tmpdir(), "wonder-claude-sessions-"));
  t.after(() => rm(dir, { recursive: true, force: true }));
  const f = await fixture(t, { claudeSessionsDir: dir });
  f.lingers.current = true;
  const { thread } = await f.bridge.request("thread/start", { cwd: f.primary, model: "claude:haiku", wonderPolicy: f.policy, wonderProject: f.project });
  f.gate.current = Promise.withResolvers();
  await f.bridge.request("turn/start", { threadId: thread.id, input: [{ type: "text", text: "Start a build" }] });
  await settle(() => f.bridge.active.get(thread.id)?.query);
  const run = f.bridge.active.get(thread.id);
  f.emits.push({ type: "system", subtype: "background_tasks_changed", tasks: [{ task_id: "bq77", task_type: "local_bash", description: "Build" }] });
  await settle(() => run.roster.tasks?.size);
  await writeFile(join(dir, `${process.pid}.json`), JSON.stringify({ pid: process.pid, procStart: processLabel(process.pid), sessionId: f.sessions.get(thread.id).sdkSessionId, status: "idle", entrypoint: "claude-desktop" }));
  f.bridge.projectMessages = f.bridge.projectTaskMessages = async () => ({ stamp: null, messages: [], desktop: false });
  f.gate.current.resolve();
  await run.finished;
  assert.ok(!f.bridge.lingering.has(thread.id));
  assert.equal(f.closes.length, 1);
});

test("Project agents may run in the background with the selected model", async () => {
  const cwd = "/tmp";
  const policy = () => new ToolPolicy({ cwd, mode: "workspace", approvalMode: "auto", readRoots: ["/"], writeRoots: [cwd], deniedRoots: [], tools: [] });
  const hook = async p => (await p.beforeTool({ tool_name: "Agent", tool_input: { prompt: "x", run_in_background: true, model: "opus" } })).hookSpecificOutput;
  const project = await hook(policy());
  assert.deepEqual(project.updatedInput, { prompt: "x", run_in_background: true }, "the parent's model, the agent's own choice of background");
});

// Owner: a Project conversation keeps its Claude process between replies, so
// the next message starts without a process start or session resume.
// A native hint arrives after result and has no turn identity of its own.
test("post-result hints stay with their completed turn across safe process reuse", async t => {
  const f = await fixture(t, { leftoverResult: false }); t.after(() => f.bridge.close());
  f.lingers.current = true;
  f.bridge.projectMessages = f.bridge.projectTaskMessages = async () => ({ stamp: null, messages: [], desktop: false });
  const { thread } = await f.bridge.request("thread/start", { cwd: f.primary, model: "claude:haiku", wonderPolicy: f.policy, wonderProject: f.project });
  const hints = () => f.frames.filter(frame => frame.params?.item?.type === "promptSuggestion");
  for (const index of [0, 1, 2]) {
    const { turn } = await f.bridge.request("turn/start", { threadId: thread.id, input: [{ type: "text", text: `Message ${index}` }] });
    await f.bridge.active.get(thread.id)?.finished;
    if (index === 0) { assert.equal(hints().length, 0, "native first turn is suppressed without blocking the second"); continue; }
    assert.equal(hints().length, index - 1, "result needs no hint to finish");
    f.emits.push({ type: "prompt_suggestion", session_id: "wrong-session", uuid: "wrong", suggestion: "Never show" });
    f.emits.push({ type: "prompt_suggestion", session_id: f.sessions.get(thread.id).sdkSessionId,
      uuid: `hint-${index}`, suggestion: `Check fixture ${index}` });
    await settle(() => hints().length === index);
    assert.equal(hints().at(-1).params.turnId, turn.id);
    assert.equal(f.sessions.get(thread.id).turns.at(-1).status, "completed");
    assert.equal(f.sessions.get(thread.id).turns.at(-1).items.at(-1).text, `Check fixture ${index}`);
    assert.equal(f.frames.filter(frame => frame.method === "turn/completed").length, index + 1, "a hint never completes or reopens a turn");
    f.emits.push({ type: "prompt_suggestion", session_id: f.sessions.get(thread.id).sdkSessionId,
      uuid: `duplicate-${index}`, suggestion: "Duplicate" });
    await new Promise(resolve => setImmediate(resolve));
    assert.equal(hints().length, index);
  }
  assert.equal(f.capturedOptions.length, 1, "hints use the retained process");
  assert.equal(f.inputs.length, 3, "no generation prompt was added");
  f.emits.push({ type: "assistant", message: { id: "background-continuation", content: [{ type: "text", text: "The background task finished." }] } });
  await settle(() => f.frames.some(frame => frame.params?.item?.text === "The background task finished."));
  const session = f.sessions.get(thread.id), latest = session.turns.at(-1);
  assert.equal(latest.items.some(item => item.type === "promptSuggestion"), false, "continuation clears the retained source hint");
  const hydrated = nativeTurns([{ type: "user", uuid: latest.id, message: { content: "Message 2" } },
    { type: "assistant", message: { id: "background-continuation", content: [{ type: "text", text: "The background task finished." }] } }], session.turns);
  assert.equal(hydrated.at(-1).items.some(item => item.type === "promptSuggestion"), false, "history cannot resurrect it");
  assert.equal(f.frames.filter(frame => frame.method === "turn/completed").length, 3);
});

test("a new message fences an unresolved old hint, including after the newer result", async t => {
  for (const afterResult of [false, true]) {
    const f = await fixture(t, { leftoverResult: false }); t.after(() => f.bridge.close());
    f.lingers.current = true;
    f.bridge.projectMessages = f.bridge.projectTaskMessages = async () => ({ stamp: null, messages: [], desktop: false });
    const { thread } = await f.bridge.request("thread/start", { cwd: f.primary, model: "claude:haiku", wonderPolicy: f.policy, wonderProject: f.project });
    await f.bridge.request("turn/start", { threadId: thread.id, input: [{ type: "text", text: "Native first turn" }] });
    await f.bridge.active.get(thread.id)?.finished;
    await f.bridge.request("turn/start", { threadId: thread.id, input: [{ type: "text", text: "First" }] });
    await f.bridge.active.get(thread.id)?.finished;
    const held = Promise.withResolvers(); f.gate.current = held;
    await f.bridge.request("turn/start", { threadId: thread.id, input: [{ type: "text", text: "Next" }] });
    await settle(() => f.inputs.length === 3);
    if (afterResult) { held.resolve(); await f.bridge.active.get(thread.id)?.finished; }
    f.emits.push({ type: "prompt_suggestion", session_id: f.sessions.get(thread.id).sdkSessionId,
      uuid: "late-old-hint", suggestion: "Belongs to the first message" });
    await new Promise(resolve => setImmediate(resolve));
    held.resolve(); await f.bridge.active.get(thread.id)?.finished;
    await new Promise(resolve => setImmediate(resolve));
    assert.equal(f.frames.filter(frame => frame.params?.item?.type === "promptSuggestion").length, 0);
    assert.equal(f.capturedOptions.length, 1, "Send proceeded immediately in the same process");
    assert.equal(f.sessions.get(thread.id).turns.length, 3);
    assert.equal(f.sessions.get(thread.id).turns.at(-1).status, "completed");
  }
});

test("a hint publication crossing the next completed turn cannot resume the older iterator consumer", async t => {
  const f = await fixture(t, { leftoverResult: false }); t.after(() => f.bridge.close());
  f.lingers.current = true;
  f.bridge.projectMessages = f.bridge.projectTaskMessages = async () => ({ stamp: null, messages: [], desktop: false });
  const { thread } = await f.bridge.request("thread/start", { cwd: f.primary, model: "claude:haiku", wonderPolicy: f.policy, wonderProject: f.project });
  const send = text => f.bridge.request("turn/start", { threadId: thread.id, input: [{ type: "text", text }] });
  for (const text of ["First suppressed", "Eligible"]) { await send(text); await f.bridge.active.get(thread.id)?.finished; }
  const publishing = Promise.withResolvers(), release = Promise.withResolvers(), originalSend = f.bridge.send;
  f.bridge.send = async frame => {
    if (frame.params?.item?.type === "promptSuggestion") { publishing.resolve(); await release.promise; }
    await originalSend(frame);
  };
  f.emits.push({ type: "prompt_suggestion", session_id: f.sessions.get(thread.id).sdkSessionId, uuid: "hint", suggestion: "Check it" });
  await publishing.promise;
  const handle = f.bridge.lingering.get(thread.id), next = handle.iterator.next.bind(handle.iterator);
  let readers = 0, maximumReaders = 0;
  handle.iterator.next = async () => {
    readers++; maximumReaders = Math.max(maximumReaders, readers);
    try { return await next(); } finally { readers--; }
  };
  await send("Newer"); await f.bridge.active.get(thread.id)?.finished;
  release.resolve();
  await new Promise(resolve => setImmediate(resolve));
  f.emits.push({ type: "system", subtype: "status", status: null });
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(maximumReaders, 1, "only the latest linger owner may keep consuming");
  assert.equal(f.inputs.length, 3);
});

test("the next reply reuses the warm process unless someone else wrote the transcript", async t => {
  const f = await fixture(t);
  f.lingers.current = true;
  const { thread } = await f.bridge.request("thread/start", { cwd: f.primary, model: "claude:haiku", wonderPolicy: f.policy, wonderProject: f.project });
  f.bridge.projectMessages = f.bridge.projectTaskMessages = async () => ({ stamp: null, messages: [], desktop: false });
  for (const text of ["One", "Two"]) {
    await f.bridge.request("turn/start", { threadId: thread.id, input: [{ type: "text", text }] });
    await f.bridge.active.get(thread.id)?.finished;
  }
  assert.equal(f.capturedOptions.length, 1, "one process for both replies");
  assert.equal(f.closes.length, 0);
  assert.deepEqual(f.inputs.map(i => i.message.content[0].text), ["One", "Two"]);
  // A warm process holds no work: Reconnect and updates may end it.
  assert.ok(f.bridge.lingering.has(thread.id));
  assert.deepEqual((await f.bridge.request("thread/loaded/list", {})).data, []);
  // Claude on the Mac (or another app) appended a turn this process never saw:
  // continuing its in-memory copy would fork the chat, so a new process resumes.
  f.sessions.get(thread.id).sdkStarted = true;
  f.bridge.sessionFiles.latestMessageUuid = async () => "written-elsewhere";
  await f.bridge.request("turn/start", { threadId: thread.id, input: [{ type: "text", text: "Three" }] });
  await f.bridge.active.get(thread.id)?.finished;
  assert.equal(f.capturedOptions.length, 2);
  assert.equal(f.capturedOptions[1].resume, f.sessions.get(thread.id).sdkSessionId);
  assert.equal(f.closes.length, 1);
  await f.bridge.close();
});

test("at most maxWarm idle processes stay up; the least recently used ends", async t => {
  const f = await fixture(t);
  f.lingers.current = true;
  f.bridge.maxWarm = 1;
  f.bridge.projectMessages = f.bridge.projectTaskMessages = async () => ({ stamp: null, messages: [], desktop: false });
  const threads = [];
  for (const text of ["A", "B"]) {
    const { thread } = await f.bridge.request("thread/start", { cwd: f.primary, model: "claude:haiku", wonderPolicy: f.policy, wonderProject: f.project });
    threads.push(thread.id);
    await f.bridge.request("turn/start", { threadId: thread.id, input: [{ type: "text", text }] });
    await f.bridge.active.get(thread.id)?.finished;
  }
  await settle(() => f.closes.length);
  assert.deepEqual([...f.bridge.lingering.keys()], [threads[1]]);
  await f.bridge.close();
});

// Whether a kept process still runs background work is decided once: work
// known only from the transcript (no live roster yet) protects it from
// eviction and from the idle timer, as it does for reconnecting.
test("a process whose transcript shows running work is neither evicted nor expired", async t => {
  const f = await fixture(t);
  f.lingers.current = true;
  f.bridge.maxWarm = 1;
  f.bridge.warmIdleMs = 0;
  f.bridge.projectMessages = f.bridge.projectTaskMessages = async () => ({ stamp: null, messages: [], desktop: false });
  const threads = [];
  f.bridge.projectTasks = async session => session.id === threads[0] ? [{ id: "tu-a", kind: "agent", status: "running" }] : [];
  for (const text of ["A", "B"]) {
    const { thread } = await f.bridge.request("thread/start", { cwd: f.primary, model: "claude:haiku", wonderPolicy: f.policy, wonderProject: f.project });
    threads.push(thread.id);
    await f.bridge.request("turn/start", { threadId: thread.id, input: [{ type: "text", text }] });
    await f.bridge.active.get(thread.id)?.finished;
  }
  await settle(() => !f.bridge.lingering.has(threads[1]));
  assert.ok(f.bridge.lingering.has(threads[0]), "the busy process stays");
  assert.equal(f.bridge.lingering.get(threads[0]).roster.tasks, null, "no live roster: only the transcript knows");
  await f.bridge.close();
});

// Guide: a message sent while a reply runs reaches the same process with
// priority "now"; the cut-short reply ends and the message is answered as
// its own turn, whose ID the steer returns.
test("Guide steers a running Project reply into the same process as its own turn", async t => {
  const f = await fixture(t);
  f.lingers.current = true;
  const { thread } = await f.bridge.request("thread/start", { cwd: f.primary, model: "claude:haiku", wonderPolicy: f.policy, wonderProject: f.project });
  f.bridge.projectMessages = f.bridge.projectTaskMessages = async () => ({ stamp: null, messages: [], desktop: false });
  f.gate.current = Promise.withResolvers();
  const { turn } = await f.bridge.request("turn/start", { threadId: thread.id, input: [{ type: "text", text: "Long answer" }] });
  await settle(() => f.bridge.active.get(thread.id)?.query);
  const steered = f.bridge.active.get(thread.id);
  await assert.rejects(f.bridge.request("turn/steer", { threadId: thread.id, expectedTurnId: "other", input: [{ type: "text", text: "x" }] }), /already finished/);
  await f.bridge.receive({ id: "rejected-guide", method: "turn/steer", params: { threadId: thread.id,
    expectedTurnId: "other", input: [{ type: "text", text: "x" }] } });
  assert.deepEqual(f.frames.find(frame => frame.id === "rejected-guide").error.data,
    { turnSteer: { accepted: false, action: "wait" } });
  const client = "22222222-2222-4222-8222-222222222222";
  const { turnId } = await f.bridge.request("turn/steer", { threadId: thread.id, expectedTurnId: turn.id, clientUserMessageId: client,
    input: [{ type: "text", text: "Shorter please" }] });
  assert.notEqual(turnId, turn.id);
  // A replayed steer returns the same receipt and writes nothing more.
  assert.equal((await f.bridge.request("turn/steer", { threadId: thread.id, expectedTurnId: turn.id, clientUserMessageId: client,
    input: [{ type: "text", text: "Shorter please" }] })).turnId, turnId);
  f.gate.current.resolve(); f.gate.current = null;
  await steered.finished;
  await settle(() => f.bridge.active.get(thread.id)?.turn.id === turnId);
  await f.bridge.active.get(thread.id)?.finished;
  assert.equal(f.capturedOptions.length, 1, "the same process answered");
  const pushed = f.inputs.find(i => i.uuid === turnId);
  assert.equal(pushed.priority, "now");
  assert.equal(pushed.message.content[0].text, "Shorter please");
  const turns = f.sessions.get(thread.id).turns;
  assert.deepEqual(turns.map(t => [t.id, t.status]), [[turn.id, "completed"], [turnId, "completed"]]);
  const completed = f.frames.filter(frame => frame.method === "turn/completed").map(frame => frame.params.turnId);
  assert.deepEqual(completed, [turn.id, turnId], "the steered reply ends first, then the Guide turn");
  await f.bridge.close();
});

// Guide admission and the reply's finalization share one owner: a Guide whose
// receipt is still being saved when the reply ends is either run or failed
// explicitly, never acknowledged with no executor left to answer it.
test("a Guide saved while its reply finishes is still answered or failed", async t => {
  const f = await fixture(t);
  f.lingers.current = true;
  const { thread } = await f.bridge.request("thread/start", { cwd: f.primary, model: "claude:haiku", wonderPolicy: f.policy, wonderProject: f.project });
  f.bridge.projectMessages = f.bridge.projectTaskMessages = async () => ({ stamp: null, messages: [], desktop: false });
  f.gate.current = Promise.withResolvers();
  const { turn } = await f.bridge.request("turn/start", { threadId: thread.id, input: [{ type: "text", text: "Long answer" }] });
  await settle(() => f.bridge.active.get(thread.id)?.query);
  const steered = f.bridge.active.get(thread.id);
  // Hold the Guide's durable acceptance while the reply runs to its end.
  const accepting = Promise.withResolvers(), accept = f.sessions.accept.bind(f.sessions);
  f.sessions.accept = async (...args) => { if (args[2]?.steering) await accepting.promise; return accept(...args); };
  const guide = f.bridge.request("turn/steer", { threadId: thread.id, expectedTurnId: turn.id, input: [{ type: "text", text: "Shorter please" }] })
    .then(receipt => ({ receipt }), error => ({ error }));
  await new Promise(resolve => setTimeout(resolve, 10));
  f.gate.current.resolve(); f.gate.current = null;
  await settle(() => f.sessions.get(thread.id).turns[0].status !== "inProgress");
  accepting.resolve();
  const outcome = await guide;
  await steered.finished;
  await settle(() => !f.bridge.active.has(thread.id) || f.bridge.active.get(thread.id).turn.id !== turn.id);
  await f.bridge.active.get(thread.id)?.finished;
  if (outcome.receipt) {
    const saved = f.sessions.get(thread.id).turns.find(t => t.id === outcome.receipt.turnId);
    assert.notEqual(saved.status, "inProgress", "an acknowledged Guide turn is answered or failed");
    assert.ok(f.frames.some(frame => frame.method === "turn/completed" && frame.params.turnId === saved.id), "its end is reported");
  }
  await f.bridge.close();
});

// Owner (Send now, Oct 12): a Guide sent during a long command waited in
// Claude's queue; Stop then named the Guide's turn, which no run owned, so
// nothing stopped, the phone showed it stopped, and Claude answered it later
// while Cupola believed the chat idle. Measured (claude-agent-sdk 0.3.283,
// haiku, a Bash `sleep` running): while a priority "now" message waits,
// query.interrupt() is ignored and the waiting message is answered anyway.
test("Stop on a Guide Claude has not taken yet ends the reply and the Guide, which is never answered", async t => {
  const f = await fixture(t);
  f.lingers.current = true;
  const { thread } = await f.bridge.request("thread/start", { cwd: f.primary, model: "claude:haiku", wonderPolicy: f.policy, wonderProject: f.project });
  f.bridge.projectMessages = f.bridge.projectTaskMessages = async () => ({ stamp: null, messages: [], desktop: false });
  f.gate.current = Promise.withResolvers();
  const { turn } = await f.bridge.request("turn/start", { threadId: thread.id, input: [{ type: "text", text: "Run the long command" }] });
  await settle(() => f.bridge.active.get(thread.id)?.query);
  const run = f.bridge.active.get(thread.id);
  const { turnId } = await f.bridge.request("turn/steer", { threadId: thread.id, expectedTurnId: turn.id,
    clientUserMessageId: "33333333-3333-4333-8333-333333333333", input: [{ type: "text", text: "Stop that, do this" }] });
  let ignored = 0;
  run.query.interrupt = async () => { ignored++; }; // As measured while a "now" message waits.
  // The phone's current turn is the Guide's, so Stop names it.
  await f.bridge.request("turn/interrupt", { threadId: thread.id, turnId });
  await Promise.race([run.finished, new Promise((_, reject) => setTimeout(() => reject(new Error("the reply kept running")), 2000))]);
  assert.ok(!f.bridge.active.has(thread.id), "the reply stopped");
  assert.equal(ignored, 0, "no soft interrupt Claude would ignore");
  const turns = f.sessions.get(thread.id).turns;
  assert.deepEqual(turns.map(t => [t.id, t.status]), [[turn.id, "interrupted"], [turnId, "interrupted"]]);
  const completed = f.frames.filter(frame => frame.method === "turn/completed").map(frame => [frame.params.turnId, frame.params.turn.status]);
  assert.deepEqual(completed, [[turn.id, "interrupted"], [turnId, "interrupted"]]);
  assert.ok(!f.inputs.some(i => i.uuid === turnId), "Claude never read the stopped Guide");
  // The next message starts a reply of its own.
  await f.bridge.request("turn/start", { threadId: thread.id, input: [{ type: "text", text: "Continue" }] });
  await f.bridge.active.get(thread.id)?.finished;
  assert.equal(f.sessions.get(thread.id).turns.at(-1).status, "completed");
  await f.bridge.close();
});

// Owner (Oct 12): after a Stop that did not land, the owner continued the chat
// in Claude on the Mac while Cupola's reply was still running: two processes
// then wrote one transcript, interleaving two branches. Last sender wins: a
// reply the Mac starts after Cupola's ends Cupola's.
test("a running reply yields when Claude on the Mac starts a reply in the chat, so one process writes it", async t => {
  const dir = await mkdtemp(join(tmpdir(), "wonder-claude-sessions-"));
  t.after(() => rm(dir, { recursive: true, force: true }));
  const f = await fixture(t, { claudeSessionsDir: dir });
  f.lingers.current = true;
  const { thread } = await f.bridge.request("thread/start", { cwd: f.primary, model: "claude:haiku", wonderPolicy: f.policy, wonderProject: f.project });
  f.bridge.projectMessages = f.bridge.projectTaskMessages = async () => ({ stamp: null, messages: [], desktop: false });
  f.gate.current = Promise.withResolvers();
  const { turn } = await f.bridge.request("turn/start", { threadId: thread.id, input: [{ type: "text", text: "Long" }] });
  await settle(() => f.bridge.active.get(thread.id)?.query);
  const run = f.bridge.active.get(thread.id);
  const sessionId = f.sessions.get(thread.id).sdkSessionId;
  const record = (status, statusUpdatedAt) => writeFile(join(dir, `${process.pid}.json`),
    JSON.stringify({ pid: process.pid, procStart: processLabel(process.pid), sessionId, status, entrypoint: "claude-desktop", statusUpdatedAt }));
  // Open there, or busy with work it started before Cupola's reply: Cupola continues.
  await record("idle", Date.now());
  await new Promise(resolve => setTimeout(resolve, 80));
  await record("busy", Date.now() - 60_000);
  await new Promise(resolve => setTimeout(resolve, 80));
  assert.equal(f.bridge.active.get(thread.id), run, "still running");
  // The owner sends from Claude on the Mac.
  await record("busy", Date.now());
  await Promise.race([run.finished, new Promise((_, reject) => setTimeout(() => reject(new Error("Cupola kept writing")), 2000))]);
  assert.ok(f.closes.length >= 1, "Cupola's process ended");
  assert.ok(!f.bridge.lingering.has(thread.id));
  const saved = f.sessions.get(thread.id).turns.find(t => t.id === turn.id);
  assert.equal(saved.status, "interrupted");
  assert.match(saved.error.message, /Claude on your computer/);
});

// A turn the journal still says is running, with no process running it (its
// run ended without recording the end), must not refuse every later message.
test("a turn left in progress with no process running it is closed, not a reason to refuse the next message", async t => {
  const f = await fixture(t);
  const { thread } = await f.bridge.request("thread/start", { cwd: f.primary, model: "claude:haiku", wonderPolicy: f.policy, wonderProject: f.project });
  f.bridge.projectMessages = f.bridge.projectTaskMessages = async () => ({ stamp: null, messages: [], desktop: false });
  const session = f.sessions.get(thread.id);
  const stale = { id: "stale-turn", clientUserMessageId: null, status: "inProgress", error: null, items: [{ type: "userMessage", id: "u", content: [] }] };
  session.turns.push(stale);
  await f.sessions.save(session);
  assert.equal(f.sessions.describe(session).status.type, "active");
  // Stop on it records its end, as the phone shows.
  await f.bridge.request("turn/interrupt", { threadId: thread.id, turnId: "stale-turn" });
  assert.equal(stale.status, "interrupted");
  assert.ok(f.frames.some(frame => frame.method === "turn/completed" && frame.params.turnId === "stale-turn"));
  stale.status = "inProgress"; f.frames.length = 0;
  const { turn } = await f.bridge.request("turn/start", { threadId: thread.id, input: [{ type: "text", text: "Next" }] });
  await f.bridge.active.get(thread.id)?.finished;
  assert.deepEqual(session.turns.map(t => [t.id, t.status]), [["stale-turn", "interrupted"], [turn.id, "completed"]]);
  const completed = f.frames.filter(frame => frame.method === "turn/completed").map(frame => frame.params.turnId);
  assert.deepEqual(completed, ["stale-turn", turn.id], "the stale turn's end is reported first");
  assert.equal(f.sessions.describe(session).status.type, "idle");
});

// A phone send that takes over a reply from Claude on the Mac is read by the
// phone's activity polling while the new turn is still being saved. That read
// must not close the turn being admitted as stale: the phone would show it
// interrupted and stop following a reply that is running.
test("a read while a new turn is being saved does not close that turn as stale", async t => {
  const f = await fixture(t);
  const { thread } = await f.bridge.request("thread/start", { cwd: f.primary, model: "claude:haiku", wonderPolicy: f.policy, wonderProject: f.project });
  f.bridge.projectMessages = f.bridge.projectTaskMessages = async () => ({ stamp: null, messages: [], desktop: false });
  const session = f.sessions.get(thread.id);
  const save = f.sessions.save.bind(f.sessions), held = Promise.withResolvers(), saving = Promise.withResolvers();
  f.sessions.save = async s => {
    if (s.turns.some(t => t.status === "inProgress") && !f.bridge.active.has(s.id)) { saving.resolve(); await held.promise; }
    return save(s);
  };
  const started = f.bridge.request("turn/start", { threadId: thread.id, clientUserMessageId: "phone-send", input: [{ type: "text", text: "Next" }] });
  await saving.promise;
  const read = f.bridge.request("thread/read", { threadId: thread.id });
  await new Promise(resolve => setTimeout(resolve, 20));
  held.resolve(); f.sessions.save = save;
  const { turn } = await started;
  await read;
  assert.ok(!f.frames.some(frame => frame.method === "turn/completed" && frame.params.turnId === turn.id && frame.params.turn.status === "interrupted"),
    "the turn being admitted is not reported interrupted");
  await f.bridge.active.get(thread.id)?.finished;
  assert.deepEqual(session.turns.map(t => [t.id, t.status, t.error]), [[turn.id, "completed", null]]);
  assert.deepEqual(f.frames.filter(frame => frame.method === "turn/completed").map(frame => [frame.params.turnId, frame.params.turn.status]),
    [[turn.id, "completed"]]);
});

// While Cupola's reply runs, a plain message must wait in Cupola's queue. The
// refusal names a running reply so the daemon keeps the message queued.
test("a message while Cupola's reply runs is refused as a running turn, before anything is written", async t => {
  const f = await fixture(t);
  const { thread } = await f.bridge.request("thread/start", { cwd: f.primary, model: "claude:haiku", wonderPolicy: f.policy, wonderProject: f.project });
  f.bridge.projectMessages = f.bridge.projectTaskMessages = async () => ({ stamp: null, messages: [], desktop: false });
  f.gate.current = Promise.withResolvers();
  await f.bridge.request("turn/start", { threadId: thread.id, input: [{ type: "text", text: "Long" }] });
  await settle(() => f.bridge.active.get(thread.id)?.query);
  await assert.rejects(f.bridge.request("turn/start", { threadId: thread.id, clientUserMessageId: "44444444-4444-4444-8444-444444444444",
    input: [{ type: "text", text: "Continue" }] }), /already has an active turn/);
  await f.bridge.receive({ id: "rejected-send", method: "turn/start", params: { threadId: thread.id,
    clientUserMessageId: "55555555-5555-4555-8555-555555555555", input: [{ type: "text", text: "Continue" }] } });
  assert.deepEqual(f.frames.find(frame => frame.id === "rejected-send").error.data,
    { turnStart: { accepted: false, action: "wait" } }, "the admission point proves rejection independently of wording");
  assert.equal(f.sessions.get(thread.id).turns.length, 1, "nothing was accepted");
  f.gate.current.resolve(); f.gate.current = null;
  await f.bridge.active.get(thread.id)?.finished;
  await f.bridge.close();
});

// Owner (send failures, Oct 12): the running count showed 6 while one task
// ran. Five were started by Claude processes that had since exited; Claude
// never reports tasks killed with their process, and two outputs already
// ended "[exited with code 0]". Only the process holding the session now can
// be running anything.
test("a restarted session's stale tasks are not running; only the current process's tasks are", async t => {
  const f = await fixture(t, { transcripts: {} });
  const dir = await mkdtemp(join(tmpdir(), "wonder-claude-sessions-"));
  const tasksRoot = await mkdtemp(join(tmpdir(), "wonder-claude-tasks-"));
  t.after(() => Promise.all([rm(dir, { recursive: true, force: true }), rm(tasksRoot, { recursive: true, force: true })]));
  const sdkSessionId = "11111111-2222-3333-4444-555555555557";
  const launch = (id, at, input, name = "Bash") => [
    { type: "assistant", uuid: `a-${id}`, timestamp: at, message: { id: `m-${id}`, stop_reason: "tool_use", content: [
      { type: "tool_use", id, name, input } ] } },
    { type: "user", uuid: `r-${id}`, timestamp: at, message: { content: [{ type: "tool_result", tool_use_id: id,
      content: name === "Bash" ? `Command running in background with ID: ${id.replace("tu-", "")}` : `Async agent launched successfully.\nagentId: ${id.replace("tu-", "")}` }] } }];
  const messages = [
    { type: "user", uuid: "turn-1", origin: { kind: "human" }, message: { content: "Go" } },
    ...launch("tu-bold1", "2026-10-11T19:55:13Z", { command: "make", description: "Old build", run_in_background: true }),
    ...launch("tu-aold2", "2026-10-11T13:10:51Z", { description: "Old audit", prompt: "Look", run_in_background: true }, "Agent"),
    ...launch("tu-bexit", "2026-10-12T07:10:00Z", { command: "make test", description: "Finished test", run_in_background: true }),
    ...launch("tu-blive", "2026-10-12T07:20:00Z", { command: "sleep 99", description: "Live", run_in_background: true }),
    { type: "assistant", uuid: "a-end", message: { id: "m-end", stop_reason: "end_turn", content: [{ type: "text", text: "Started." }] } }];
  const sdk = { getSessionMessages: async () => messages, getSessionInfo: async (id, { dir: cwd }) => ({ sessionId: id, cwd, lastModified: 1 }) };
  const bridge = new ClaudeBridge({ updates: { acquire: async () => ({ runtime: { sdk }, release: async () => {} }) },
    sessions: f.sessions, send: async () => {}, claudeSessionsDir: dir, claudeTasksRoot: tasksRoot });
  syntheticTaskFiles(bridge, () => messages);
  t.after(() => bridge.close());
  const { thread } = await bridge.request("project/session/attach", { sessionId: sdkSessionId, model: "claude:haiku", wonderPolicy: f.policy, wonderProject: f.project });
  // The Claude process that holds the session now started at 07:00, after
  // the old ones were launched by processes that have exited.
  await writeFile(join(dir, `${process.pid}.json`), JSON.stringify({ pid: process.pid, procStart: processLabel(process.pid), sessionId: sdkSessionId, status: "idle",
    startedAt: Date.parse("2026-10-12T07:00:00Z") }));
  const output = join(tasksRoot, projectFolderName(f.primary), sdkSessionId, "tasks");
  await mkdir(output, { recursive: true });
  await writeFile(join(output, "bexit.output"), "ok\n\n[exited with code 0]\n");
  await writeFile(join(output, "blive.output"), "still going\n");
  const tasks = Object.fromEntries((await bridge.request("thread/backgroundTasks/list", { threadId: thread.id })).data.map(t => [t.id, t.status]));
  assert.deepEqual(tasks, { "tu-bold1": "interrupted", "tu-aold2": "interrupted", "tu-bexit": "completed", "tu-blive": "running" });
  // A failed command's exit code makes it failed, not running.
  await writeFile(join(output, "blive.output"), "boom\n[exited with code 2]");
  bridge.transcripts.clear(); bridge.taskProjections.clear();
  const after = (await bridge.request("thread/backgroundTasks/list", { threadId: thread.id })).data;
  assert.equal(after.find(t => t.id === "tu-blive").status, "failed");
  assert.equal(after.filter(t => t.status === "running").length, 0);
  // Claude Code ends a stopped command's output with "[killed]".
  await writeFile(join(output, "blive.output"), "partial\n\n[killed]\n");
  bridge.transcripts.clear(); bridge.taskProjections.clear();
  const killed = (await bridge.request("thread/backgroundTasks/list", { threadId: thread.id })).data;
  assert.equal(killed.find(t => t.id === "tu-blive").status, "interrupted");
});

test("a task launched before Cupola's own process started is not running in it", () => {
  const messages = [
    { type: "assistant", uuid: "a1", timestamp: "2026-10-11T10:00:00Z", message: { id: "m1", content: [
      { type: "tool_use", id: "old", name: "Bash", input: { command: "x", run_in_background: true } }] } }];
  const later = [...messages, { type: "assistant", uuid: "a2", timestamp: "2026-10-12T10:00:00Z", message: { id: "m2", content: [
    { type: "tool_use", id: "new", name: "Bash", input: { command: "y", run_in_background: true } }] } }];
  const since = Date.parse("2026-10-12T09:00:00Z");
  const status = Object.fromEntries(backgroundTasks(later, true, [], { since }).map(t => [t.id, t.status]));
  assert.deepEqual(status, { old: "interrupted", new: "running" });
  assert.deepEqual(Object.fromEntries(backgroundTasks(later, true).map(t => [t.id, t.status])), { old: "running", new: "running" },
    "without a known process start the transcript decides");
});

// Native summary is available only on an already-owned SDK Query. Reading a
// chat must not start a process or invoke the full token-count API mode.
test("owned context reads use the SDK summary once and reject stale process results", async t => {
  const f = await fixture(t);
  const { thread } = await f.bridge.request("thread/start", { cwd: f.primary, model: "claude:haiku", wonderPolicy: f.policy, wonderProject: f.project });
  const read = () => f.bridge.request("thread/read", { threadId: thread.id, includeContextUsage: true });
  assert.equal((await read()).contextUsage, undefined);
  assert.equal(f.capturedOptions.length, 0);
  f.lingers.current = true;
  await f.bridge.request("turn/start", { threadId: thread.id, input: [{ type: "text", text: "Fixture" }] });
  await new Promise(resolve => setImmediate(resolve));
  await f.bridge.active.get(thread.id)?.finished;
  const session = f.sessions.get(thread.id), handle = f.bridge.processHandle(session);
  assert.ok(handle);
  const calls = [], gate = Promise.withResolvers();
  handle.query.getContextUsage = options => { calls.push(options); return gate.promise; };
  assert.equal((await f.bridge.request("thread/read", { threadId: thread.id })).contextUsage, undefined);
  assert.deepEqual(calls, [], "status-only admission reads do not request context usage");
  const first = read(), second = read();
  await new Promise(resolve => setImmediate(resolve));
  assert.deepEqual(calls, [{ detail: "summary" }]);
  gate.resolve({ totalTokens: 1_234, rawMaxTokens: 200_000, model: "claude-native" });
  const responses = await Promise.all([first, second]);
  assert.deepEqual(responses[0].contextUsage, responses[1].contextUsage);
  assert.equal(responses[0].contextUsage.tokenUsage.last.totalTokens, 1_234);
  assert.deepEqual((await read()).contextUsage, responses[0].contextUsage);
  assert.equal(calls.length, 1, "unchanged native state reuses its measured snapshot and clock");
  f.bridge.observeContextUsage(session, handle, { type: "system", subtype: "compact_boundary" });
  const slow = Promise.withResolvers();
  handle.query.getContextUsage = options => { calls.push(options); return slow.promise; };
  const timedOut = await read();
  assert.equal(timedOut.contextUsage, null);
  assert.equal((await read()).contextUsage, null);
  assert.equal(calls.length, 2, "a timeout must not multiply an SDK operation that is still pending");
  slow.resolve({ totalTokens: 900, rawMaxTokens: 200_000, model: "claude-native" });
  await new Promise(resolve => setImmediate(resolve));
  assert.equal((await read()).contextUsage.tokenUsage.last.totalTokens, 900);
  f.bridge.observeContextUsage(session, handle, { type: "user" });
  const stale = Promise.withResolvers();
  handle.query.getContextUsage = () => stale.promise;
  const pending = read();
  await new Promise(resolve => setImmediate(resolve));
  // A native /context response arriving meanwhile supersedes the control read.
  f.bridge.observeContextUsage(session, handle, { type: "assistant", context_usage:
    { total_tokens: 600, raw_max_tokens: 180_000, model: "claude-native" } });
  stale.resolve({ totalTokens: 850, rawMaxTokens: 200_000, model: "claude-native" });
  assert.equal((await pending).contextUsage, null);
  const native = await read();
  assert.equal(native.contextUsage.tokenUsage.last.totalTokens, 600);
  assert.equal(native.contextUsage.tokenUsage.modelContextWindow, 180_000);
  f.bridge.observeContextUsage(session, handle, { type: "user" });
  const replaced = Promise.withResolvers();
  handle.query.getContextUsage = () => replaced.promise;
  const abandoned = read();
  await new Promise(resolve => setImmediate(resolve));
  f.bridge.lingering.delete(thread.id);
  replaced.resolve({ totalTokens: 500, rawMaxTokens: 200_000, model: "claude-native" });
  assert.equal((await abandoned).contextUsage, null);
  assert.equal((await read()).contextUsage, undefined);
  f.bridge.lingering.set(thread.id, handle); // The fixture owner closes exactly its process.
});


test("native clear waits for durable binding acknowledgement and retains accepted receipts", async t => {
  const f = await fixture(t);
  f.lingers.current = true;
  f.gate.current = Promise.withResolvers();
  const { thread } = await f.bridge.request("thread/start", { cwd: f.primary, model: "claude:haiku",
    wonderPolicy: f.policy, wonderProject: f.project });
  const { turn } = await f.bridge.request("turn/start", { threadId: thread.id, clientUserMessageId: "clear-request",
    input: [{ type: "text", text: "/clear" }] });
  await settle(() => f.inputs.length === 1);
  const session = f.sessions.get(thread.id), oldId = session.sdkSessionId;
  const nativeId = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
  const hook = f.capturedOptions[0].hooks.SessionStart?.[0].hooks[0];
  assert.equal(typeof hook, "function", "native SessionStart owns resumable identity");
  await hook({ hook_event_name: "SessionStart", source: "clear", session_id: nativeId });
  const recovered = await new Sessions(join(f.root, "sessions")).initialize();
  assert.equal((await recovered.load(thread.id)).pendingReset.newSessionId, nativeId, "hook acknowledges only durable pending identity");
  assert.equal(session.sdkSessionId, oldId, "binding stays old until daemon acknowledgement");
  f.emits.push({ type: "conversation_reset", uuid: "reset-frame", session_id: oldId,
    new_conversation_id: "11111111-2222-3333-4444-555555555555", user_message_uuid: turn.id });
  f.emits.push({ type: "result", subtype: "success", user_message_uuid: turn.id, session_id: nativeId });
  await settle(() => f.frames.some(frame => frame.method === "thread/session/reset"));
  const reset = f.frames.find(frame => frame.method === "thread/session/reset");
  assert.ok(reset);
  assert.equal(reset.params.oldSessionId, oldId);
  assert.equal(reset.params.sessionId, nativeId, "display conversation UUID is never resumable identity");
  assert.ok(f.frames.findIndex(frame => frame.method === "turn/completed") < f.frames.indexOf(reset), "old terminal stream precedes reset");
  await assert.rejects(f.bridge.request("turn/start", { threadId: thread.id, input: [] }), /reset/i);
  await f.bridge.receive({ id: reset.id, result: { sessionId: nativeId, historyRevision: 1 } });
  await settle(() => session.sdkSessionId === nativeId && !session.pendingReset);
  assert.equal(session.sdkSessionId, nativeId);
  assert.equal(session.turns.length, 0);
  assert.equal(session.receipts["clear-request"].id, turn.id);
  assert.equal((await f.bridge.request("turn/start", { threadId: thread.id, clientUserMessageId: "clear-request", input: [] })).turn.id, turn.id);
  f.gate.current.resolve();
  await f.bridge.active.get(thread.id)?.finished;
  f.gate.current = null;
  await f.bridge.request("turn/start", { threadId: thread.id, input: [{ type: "text", text: "After reset" }] });
  await f.bridge.active.get(thread.id)?.finished;
  assert.equal(f.inputs.at(-1).session_id, nativeId, "next Send uses the confirmed new native identity");
  assert.equal(f.capturedOptions.length, 1, "reset retains the original Query");
});

// Contract: only the existing native Query owns session commands. Discovery
// must never create a second process, alter history, or prolong warm ownership.
test("session command catalogs are unavailable cold and preserve native rows on the retained Query", async t => {
  const f = await fixture(t);
  const { thread } = await f.bridge.request("thread/start", { cwd: f.primary, model: "claude:haiku", wonderPolicy: f.policy, wonderProject: f.project });
  const read = () => f.bridge.request("thread/commands/list", { threadId: thread.id });
  f.bridge.inspect = () => assert.fail("command discovery cannot inspect another Query");
  assert.equal((await read()).availability, "unavailable");
  assert.equal(f.capturedOptions.length, 0);
  f.lingers.current = true;
  await f.bridge.request("turn/start", { threadId: thread.id, input: [{ type: "text", text: "Fixture" }] });
  await f.bridge.active.get(thread.id)?.finished;
  const session = f.sessions.get(thread.id), handle = f.bridge.processHandle(session);
  const saved = structuredClone(session), lastActive = handle.lastActive;
  const rows = [
    { name: "usage", description: "Native usage", argumentHint: "", aliases: ["cost", "stats"], builtin: true },
    { name: "usage", description: "Project command", argumentHint: "<file>", aliases: ["project-usage"] },
  ];
  let calls = 0;
  handle.query.supportedCommands = async () => { calls++; return rows; };
  const value = await read();
  assert.equal(value.availability, "ready");
  assert.deepEqual(value.commands, rows);
  value.commands[0].aliases.pop();
  assert.deepEqual((await read()).commands, rows, "callers cannot mutate native rows");
  assert.equal(calls, 1);
  assert.deepEqual(session, saved, "catalogs never enter the durable transcript");
  assert.equal(handle.lastActive, lastActive);
  assert.equal(f.capturedOptions.length, 1);
  assert.equal(f.inputs.length, 1);
  await f.bridge.endLinger(thread.id, false);
  assert.equal((await read()).availability, "unavailable");
});

const nativeCommand = name => ({ name, description: `Native ${name}`, argumentHint: "<file>" });
async function commandsFixture(t, active = false) {
  const f = await fixture(t);
  f.lingers.current = true;
  f.bridge.projectMessages = f.bridge.projectTaskMessages = async () => ({ stamp: null, messages: [], desktop: false });
  const { thread } = await f.bridge.request("thread/start", { cwd: f.primary, model: "claude:haiku", wonderPolicy: f.policy, wonderProject: f.project });
  if (active) f.gate.current = Promise.withResolvers();
  await f.bridge.request("turn/start", { threadId: thread.id, input: [{ type: "text", text: "Fixture" }] });
  await settle(() => f.inputs.length === 1);
  if (!active) await f.bridge.active.get(thread.id)?.finished;
  const session = f.sessions.get(thread.id), handle = f.bridge.processHandle(session);
  return { ...f, thread, session, handle, read: () => f.bridge.request("thread/commands/list", { threadId: thread.id }),
    push: (commands, sessionId = session.sdkSessionId) => f.emits.push({ type: "system", subtype: "commands_changed", session_id: sessionId, commands }) };
}

test("session command reads share one bounded SDK operation and newer native pushes win", async t => {
  const f = await commandsFixture(t, true);
  let calls = 0;
  const pending = Promise.withResolvers();
  f.handle.query.supportedCommands = () => { calls++; return pending.promise; };
  const first = f.read(), second = f.read();
  assert.equal((await first).availability, "unknown");
  assert.equal((await second).availability, "unknown");
  assert.equal((await f.read()).availability, "unknown");
  assert.equal(calls, 1, "timeouts do not create another read");
  f.push([nativeCommand("fresh")]);
  await settle(() => f.handle.commands.commands?.[0]?.name === "fresh");
  pending.resolve([nativeCommand("old-initialization")]);
  await new Promise(resolve => setImmediate(resolve));
  assert.deepEqual((await f.read()).commands, [nativeCommand("fresh")]);
  assert.equal(calls, 1);
  f.gate.current.resolve();
  await f.bridge.active.get(f.thread.id)?.finished;
  const lastActive = f.handle.lastActive, revision = (await f.read()).revision;
  f.push([]);
  await settle(() => f.handle.commands.revision > revision);
  assert.equal((await f.read()).availability, "ready");
  assert.deepEqual((await f.read()).commands, []);
  assert.equal(f.handle.lastActive, lastActive, "idle catalog updates do not prolong the Query");
  f.push([nativeCommand("foreign")], "another-session");
  f.emits.push({ type: "system", subtype: "commands_changed", session_id: f.session.sdkSessionId,
    parent_tool_use_id: "child", commands: [nativeCommand("child")] });
  await new Promise(resolve => setImmediate(resolve));
  assert.deepEqual((await f.read()).commands, []);
  assert.equal(f.frames.some(frame => JSON.stringify(frame).includes("fresh")), false, "catalogs are not transcript items");
  assert.equal(f.inputs.length, 1);
  assert.equal(f.capturedOptions.length, 1);
});

test("invalid session command replacements remove stale rows and preserve unknown versus empty", async t => {
  const f = await commandsFixture(t);
  assert.equal((await f.read()).availability, "unknown", "no supported read is not an empty native list");
  for (const invalid of [undefined, {}, [nativeCommand("")], [{ ...nativeCommand("x"), aliases: [3] }],
    [{ ...nativeCommand("x"), builtin: "yes" }], Array.from({ length: 513 }, () => nativeCommand("x")),
    Array.from({ length: 32 }, () => ({ ...nativeCommand("x"), description: "x".repeat(16384) }))]) {
    f.push([nativeCommand("before")]);
    await settle(() => f.handle.commands.commands?.[0]?.name === "before");
    const revision = f.handle.commands.revision;
    f.push(invalid);
    await settle(() => f.handle.commands.revision > revision);
    const value = await f.read();
    assert.equal(value.availability, "unknown");
    assert.equal(value.commands, null);
    assert.equal(value.reason, "invalidCatalog");
  }
  f.push([]);
  await settle(() => f.handle.commands.commands?.length === 0);
  assert.deepEqual((await f.read()).commands, []);
});

test("session command reads discard results after exact Query ownership or configuration changes", async t => {
  for (const change of ["settings", "shape", "session", "replacement", "abort", "close"]) await t.test(change, async t => {
    const f = await commandsFixture(t);
    const pending = Promise.withResolvers();
    f.handle.query.supportedCommands = () => pending.promise;
    const read = f.read();
    await new Promise(resolve => setImmediate(resolve));
    if (change === "settings") await f.bridge.request("thread/settings/update", { threadId: f.thread.id, model: "claude:sonnet" });
    if (change === "shape") f.handle.shape += "changed";
    if (change === "session") f.session.sdkSessionId = "new-native-session";
    if (change === "replacement") f.bridge.lingering.set(f.thread.id, { ...f.handle, query: {} });
    if (change === "abort") f.handle.abort.abort();
    if (change === "close") await f.bridge.endLinger(f.thread.id, false);
    pending.resolve([nativeCommand("stale")]);
    assert.equal((await read).availability, "unavailable");
    assert.equal((await f.read()).availability, "unavailable");
    if (change === "replacement") f.bridge.lingering.set(f.thread.id, f.handle); // Close only the fixture's original Query.
  });
});

test("native session identity changes wait for that session's command push", async t => {
  const f = await commandsFixture(t, true);
  const pending = Promise.withResolvers();
  let calls = 0;
  f.handle.query.supportedCommands = () => { calls++; return pending.promise; };
  const oldRead = f.read();
  await new Promise(resolve => setImmediate(resolve));
  const previousId = f.session.sdkSessionId, nextId = "11111111-2222-3333-4444-555555555558";
  f.emits.push({ type: "system", subtype: "init", session_id: nextId });
  await settle(() => f.session.sdkSessionId === nextId);
  pending.resolve([nativeCommand("old-session")]);
  assert.equal((await oldRead).availability, "unavailable");
  assert.equal((await f.read()).availability, "unknown");
  assert.equal(calls, 1, "the old initialization fallback must not describe the new native session");
  f.gate.current.resolve();
  await f.bridge.active.get(f.thread.id)?.finished;
  await f.bridge.request("thread/settings/update", { threadId: f.thread.id, model: "claude:sonnet" });
  await f.bridge.request("turn/start", { threadId: f.thread.id, input: [{ type: "text", text: "Continue fixture" }] });
  await f.bridge.active.get(f.thread.id)?.finished;
  assert.equal((await f.read()).availability, "unknown");
  assert.equal(calls, 1, "same-Query rebinding cannot restore the old initialization fallback");
  assert.equal(f.capturedOptions.length, 1);
  f.push([nativeCommand("wrong")], previousId);
  f.push([nativeCommand("new-session")], nextId);
  await settle(() => f.handle.commands.commands?.[0]?.name === "new-session");
  const value = await f.read();
  assert.equal(value.sessionId, nextId);
  assert.deepEqual(value.commands, [nativeCommand("new-session")]);
  f.gate.current.resolve();
});

test("transcript replacement, desktop takeover and idle expiry discard command ownership", async t => {
  for (const change of ["transcript", "desktop", "expiry", "update"]) await t.test(change, async t => {
    const f = await commandsFixture(t);
    f.handle.query.supportedCommands = async () => [nativeCommand("before")];
    const value = await f.read();
    assert.equal(value.availability, "ready");
    if (change === "transcript") {
      f.bridge.sessionFiles.latestMessageUuid = async () => "written-elsewhere";
      await f.bridge.request("turn/start", { threadId: f.thread.id, input: [{ type: "text", text: "Continue fixture" }] });
      await f.bridge.active.get(f.thread.id)?.finished;
      assert.equal((await f.read()).availability, "unknown");
      assert.notEqual((await f.read()).ownerId, value.ownerId);
    } else {
      if (change === "desktop") f.bridge.openOnMac = async () => true;
      if (change === "expiry") f.bridge.warmIdleMs = 0;
      if (change === "update") f.bridge.updates.pending = true;
      await settle(() => !f.bridge.lingering.has(f.thread.id));
      assert.equal((await f.read()).availability, "unavailable");
    }
    assert.equal(f.handle.commands.value().availability, "unavailable");
  });
});

test("native conversation reset hides commands until a new SDK session is identified", async t => {
  const f = await commandsFixture(t, true);
  let calls = 0;
  f.handle.query.supportedCommands = async () => { calls++; return [nativeCommand("old-session")]; };
  assert.equal((await f.read()).availability, "ready");
  const oldId = f.session.sdkSessionId;
  const displayId = "11111111-2222-3333-4444-555555555559";
  const nextId = "11111111-2222-3333-4444-555555555560";
  f.emits.push({ type: "conversation_reset", trigger: "clear", session_id: oldId, new_conversation_id: displayId });
  await settle(() => f.handle.commands.awaitingInit);
  assert.equal((await f.read()).availability, "unavailable");
  assert.equal(f.session.sdkSessionId, oldId, "a display conversation ID is not a resumable SDK session ID");
  f.push([nativeCommand("late-old")], oldId);
  await new Promise(resolve => setImmediate(resolve));
  assert.equal((await f.read()).availability, "unavailable");
  f.emits.push({ type: "system", subtype: "init", session_id: nextId });
  await settle(() => f.session.pendingReset?.newSessionId === nextId);
  assert.equal((await f.read()).availability, "unavailable", "native ID is pending durable daemon binding");
  f.gate.current.resolve();
  await settle(() => f.frames.some(frame => frame.method === "thread/session/reset"));
  const reset = f.frames.find(frame => frame.method === "thread/session/reset");
  await f.bridge.receive({ id: reset.id, result: { sessionId: nextId } });
  await f.bridge.active.get(f.thread.id)?.finished;
  assert.equal((await f.read()).availability, "unknown");
  assert.equal(calls, 1, "reset cannot revive the old SDK initialization fallback");
  f.push([nativeCommand("new-session")], nextId);
  await settle(() => f.handle.commands.commands?.[0]?.name === "new-session");
  assert.equal((await f.read()).sessionId, nextId);
  f.gate.current.resolve();
  await f.bridge.active.get(f.thread.id)?.finished;
  f.emits.push({ type: "conversation_reset", session_id: nextId, trigger: "future-trigger" });
  await settle(() => f.handle.commands.awaitingInit);
  assert.equal((await f.read()).availability, "unavailable", "idle or malformed resets never retain old capabilities");
  assert.equal(calls, 1);
});


test("no-op resume preserves command ownership while applied settings require a fresh bounded read", async t => {
  const f = await commandsFixture(t);
  let calls = 0;
  f.handle.query.supportedCommands = async () => { calls++; if (calls === 1) throw new Error("SDK unavailable"); return [nativeCommand("current")]; };
  assert.equal((await f.read()).availability, "unknown", "SDK failure is not an empty list");
  const before = await f.read();
  assert.equal(before.availability, "ready");
  await f.bridge.request("thread/resume", { threadId: f.thread.id, excludeTurns: true });
  assert.deepEqual(await f.read(), before, "a no-op resume cannot hide a warm catalog");
  await f.bridge.request("thread/settings/update", { threadId: f.thread.id, model: "claude:sonnet" });
  assert.equal((await f.read()).availability, "unavailable", "stored settings have not reached this Query yet");
  await f.bridge.request("turn/start", { threadId: f.thread.id, input: [{ type: "text", text: "Continue fixture" }] });
  await f.bridge.active.get(f.thread.id)?.finished;
  const after = await f.read();
  assert.equal(after.availability, "ready");
  assert.equal(after.ownerId, before.ownerId, "the native control reused this Query");
  assert.ok(after.revision > before.revision);
  assert.equal(calls, 3);
  assert.equal(f.capturedOptions.length, 1);
});


test("an idle clear uses the native hook without init and buffers only the new command catalog", async t => {
  const f = await commandsFixture(t);
  let calls = 0;
  f.handle.query.supportedCommands = async () => { calls++; return [nativeCommand("old")]; };
  assert.equal((await f.read()).availability, "ready");
  const old = f.session.sdkSessionId, next = "22222222-2222-3333-4444-555555555555";
  await f.capturedOptions[0].hooks.SessionStart[0].hooks[0]({ hook_event_name: "SessionStart", source: "clear", session_id: next });
  f.emits.push({ type: "conversation_reset", trigger: "clear", uuid: "reset", session_id: old,
    new_conversation_id: "33333333-2222-3333-4444-555555555555" });
  await settle(() => f.frames.some(frame => frame.method === "thread/session/reset"));
  const reset = f.frames.find(frame => frame.method === "thread/session/reset");
  assert.ok(reset);
  assert.equal((await f.read()).availability, "unavailable");
  await f.bridge.receive({ id: reset.id, result: { sessionId: next } });
  await settle(() => f.session.sdkSessionId === next);
  assert.equal((await f.read()).availability, "unknown");
  f.push([nativeCommand("new")], next);
  await settle(() => f.handle.commands.commands?.[0]?.name === "new");
  assert.deepEqual((await f.read()).commands, [nativeCommand("new")]);
  assert.equal(calls, 1, "no stale initialization fallback after native clear without init");
});

test("unknown native reset triggers preserve output and refuse new input across journal restart", async t => {
  const f = await commandsFixture(t);
  const original = structuredClone(f.session.turns), nativeId = f.session.sdkSessionId;
  f.emits.push({ type: "conversation_reset", trigger: "future-trigger", uuid: "reset", session_id: nativeId,
    new_conversation_id: "33333333-2222-3333-4444-555555555555" });
  await settle(() => f.session.pendingReset?.unsupported);
  assert.equal((await f.read()).availability, "unavailable");
  // The in-memory admission fence precedes its journal write. Restart only
  // after that write finishes; otherwise this reads the pre-reset journal.
  await f.sessions.writes.get(f.thread.id);
  const restored = await new Sessions(join(f.root, "sessions")).initialize();
  const session = (await restored.load(f.thread.id));
  assert.deepEqual(session.turns, original);
  assert.equal(session.sdkSessionId, nativeId);
  await assert.rejects(restored.accept(session, { input: [] }), error => error.action === "retry" && /Claude on your computer/.test(error.message));
  assert.equal(f.frames.some(frame => frame.method === "thread/session/reset"), false);
  await assert.rejects(f.bridge.forkProjectSession({ threadId: session.id }), /Claude on your computer/);
});

test("native clear pending acknowledgement survives rejection and restart without replaying a prompt", async t => {
  const f = await commandsFixture(t);
  const old = f.session.sdkSessionId, next = "22222222-2222-3333-4444-555555555555";
  const { turn: acceptedGuide } = await f.sessions.accept(f.session, { clientUserMessageId: "guide-before-reset", input: [] }, { steering: true });
  f.session.pendingReset = { oldSessionId: old, newSessionId: next, observed: true };
  await f.sessions.save(f.session);
  const first = f.bridge.finishNativeReset(f.session);
  await settle(() => f.frames.some(frame => frame.method === "thread/session/reset"));
  const request = f.frames.find(frame => frame.method === "thread/session/reset");
  await f.bridge.receive({ id: request.id, error: { code: -32000, message: "ACK lost" } });
  await assert.rejects(first);
  const restored = await new Sessions(join(f.root, "sessions")).initialize();
  const session = (await restored.load(f.thread.id));
  assert.equal(session.pendingReset.newSessionId, next);
  assert.equal(session.sdkSessionId, old);
  f.bridge.sessions = restored;
  const second = f.bridge.finishNativeReset(session);
  await settle(() => f.frames.filter(frame => frame.method === "thread/session/reset").length === 2);
  const retry = f.frames.filter(frame => frame.method === "thread/session/reset")[1];
  assert.deepEqual(retry.params, request.params);
  assert.deepEqual(retry.params.retainedTurnIds, [acceptedGuide.id], "restart cannot change accepted Guide ownership");
  await f.bridge.receive({ id: retry.id, result: { sessionId: next } });
  await second;
  assert.equal(session.sdkSessionId, next);
  assert.equal(f.inputs.length, 1, "recovery creates no provider work");
  assert.equal((await (await new Sessions(join(f.root, "sessions")).initialize()).load(session.id)).pendingReset, undefined);
});

test("a native history read spanning clear cannot populate the new transcript cache", async t => {
  const f = await commandsFixture(t);
  const read = Promise.withResolvers(), started = Promise.withResolvers();
  f.bridge.withSdk = async action => action({ getSessionMessages: async () => { started.resolve(); return read.promise; } });
  const before = f.session.sdkSessionId;
  const pending = ClaudeBridge.prototype.projectMessages.call(f.bridge, f.session);
  await started.promise;
  f.session.sdkSessionId = "22222222-2222-3333-4444-555555555555";
  read.resolve([{ type: "user", uuid: "old", message: { content: "old history" } }]);
  await assert.rejects(pending, /updating/);
  assert.equal(f.bridge.transcripts.has(f.session.sdkSessionId), false);
  assert.equal(f.bridge.transcripts.has(before), false);
});

test("native clear preserves an already accepted Guide through binding acknowledgement", async t => {
  const f = await commandsFixture(t, true);
  const clearing = f.bridge.active.get(f.thread.id), clearTurn = clearing.turn.id;
  const { turnId } = await f.bridge.request("turn/steer", { threadId: f.thread.id, expectedTurnId: clearTurn,
    clientUserMessageId: "accepted-guide", input: [{ type: "text", text: "Continue after clear" }] });
  const next = "22222222-2222-3333-4444-555555555555";
  await f.capturedOptions[0].hooks.SessionStart[0].hooks[0]({ hook_event_name: "SessionStart", source: "clear", session_id: next });
  f.emits.push({ type: "conversation_reset", trigger: "clear", uuid: "reset", session_id: f.session.sdkSessionId,
    user_message_uuid: clearTurn, new_conversation_id: "33333333-2222-3333-4444-555555555555" });
  f.emits.push({ type: "result", subtype: "success", user_message_uuid: clearTurn, session_id: next });
  await settle(() => f.frames.some(frame => frame.method === "thread/session/reset"));
  const request = f.frames.find(frame => frame.method === "thread/session/reset");
  assert.deepEqual(request.params.retainedTurnIds, [turnId]);
  await f.bridge.receive({ id: request.id, result: { sessionId: next } });
  await clearing.finished;
  f.gate.current.resolve(); f.gate.current = null;
  await settle(() => f.session.turns.find(turn => turn.id === turnId)?.status === "completed");
  await f.bridge.active.get(f.thread.id)?.finished;
  assert.deepEqual(f.session.turns.map(turn => [turn.id, turn.status]), [[turnId, "completed"]]);
  assert.equal(f.session.receipts["accepted-guide"].id, turnId);
  assert.equal(f.session.sdkSessionId, next);
  assert.equal(f.inputs.filter(input => input.uuid === turnId).length, 1);
});

// Edit from here. Turn ids are the uuids of the prompts that opened them.
const rewindMessages = [
  { type: "user", uuid: "turn-1", origin: { kind: "human" }, message: { content: "First ask" } },
  { type: "assistant", uuid: "a1", message: { id: "m1", content: [{ type: "text", text: "Done one" }] } },
  { type: "user", uuid: "turn-2", origin: { kind: "human" }, message: { content: "Second ask" } },
  { type: "assistant", uuid: "a2", message: { id: "m2", content: [{ type: "text", text: "Done two" }] } },
  { type: "user", uuid: "turn-3", origin: { kind: "human" }, message: { content: "Third ask" } },
  { type: "assistant", uuid: "a3", message: { id: "m3", content: [{ type: "text", text: "Done three" }] } },
];

async function rewindFixture(t) {
  const claudeSessionsDir = await mkdtemp(join(tmpdir(), "wonder-claude-sessions-"));
  t.after(() => rm(claudeSessionsDir, { recursive: true, force: true }));
  const id = "11111111-2222-4333-8444-555555555555";
  const transcripts = {};
  const f = await fixture(t, { transcripts, claudeSessionsDir });
  transcripts[id] = { cwd: f.primary, messages: rewindMessages };
  const { thread } = await f.bridge.request("project/session/attach", { sessionId: id, model: "claude:haiku", wonderPolicy: f.policy, wonderProject: f.project });
  const turnIds = async () => (await f.bridge.request("thread/turns/list", { threadId: thread.id })).data.map(turn => turn.id);
  return { ...f, id, thread, transcripts, turnIds };
}

// Contract: Edit from here cuts the conversation before the owner's message:
// history ends there at once, the next reply resumes Claude's session at the
// entry before it (so Claude drops the rest), and the cut ends once Claude's
// transcript has the new branch. Files are untouched unless asked.
test("edit from here resumes before the message and history ends there until the next reply", async t => {
  const f = await rewindFixture(t);
  assert.deepEqual(await f.turnIds(), ["turn-3", "turn-2", "turn-1"]);
  await assert.rejects(f.bridge.request("thread/rewind", { threadId: f.thread.id, turnId: "a2" }), /no longer in this conversation/);
  // No reply ran with file backups, so file restore is refused and nothing changes.
  await assert.rejects(f.bridge.request("thread/rewind", { threadId: f.thread.id, turnId: "turn-2", restoreFiles: true }), /didn't keep file backups/);
  assert.equal(f.sessions.get(f.thread.id).rewind, undefined);
  assert.equal(f.rewinds.length, 0);
  f.bridge.active.set(f.thread.id, {});
  await assert.rejects(f.bridge.request("thread/rewind", { threadId: f.thread.id, turnId: "turn-2" }), /Wait for Claude to finish/);
  f.bridge.active.delete(f.thread.id);

  const result = await f.bridge.request("thread/rewind", { threadId: f.thread.id, turnId: "turn-2" });
  assert.deepEqual(result, { removedTurnIds: ["turn-2", "turn-3"], filesRestored: false });
  assert.deepEqual(f.sessions.get(f.thread.id).rewind, { turnId: "turn-2", at: "a1" });
  assert.deepEqual(await f.turnIds(), ["turn-1"]);
  assert.equal(f.rewinds.length, 0, "no file restore was asked for");

  // The next reply resumes at the entry before the removed message.
  const { turn } = await f.bridge.request("turn/start", { threadId: f.thread.id, input: [{ type: "text", text: "Second ask, better" }] });
  await new Promise(resolve => setImmediate(resolve));
  await f.bridge.active.get(f.thread.id)?.finished;
  const options = f.capturedOptions.at(-1);
  assert.equal(options.resume, f.id);
  assert.equal(options.resumeSessionAt, "a1");
  assert.equal(options.enableFileCheckpointing, undefined, "backups are off by default");
  assert.equal(f.sessions.get(f.thread.id).rewind, undefined, "Claude answered on the new branch");
  assert.equal(f.inputs.at(-1).uuid, turn.id);
});

// Contract: a rewind of the first message keeps nothing, so the conversation
// continues in a new native session; the old transcript is left alone.
test("edit from the first message starts a fresh Claude session", async t => {
  const f = await rewindFixture(t);
  const result = await f.bridge.request("thread/rewind", { threadId: f.thread.id, turnId: "turn-1" });
  assert.deepEqual(result.removedTurnIds, ["turn-1", "turn-2", "turn-3"]);
  assert.match(result.sessionId, /^[0-9a-f-]{36}$/);
  assert.notEqual(result.sessionId, f.id);
  const session = f.sessions.get(f.thread.id);
  assert.equal(session.sdkSessionId, result.sessionId);
  assert.equal(session.sdkStarted, false);
  assert.deepEqual(await f.turnIds(), []);
  assert.deepEqual(f.transcripts[f.id].messages, rewindMessages, "the old transcript is untouched");
  await f.bridge.request("turn/start", { threadId: f.thread.id, input: [{ type: "text", text: "Start over" }] });
  await new Promise(resolve => setImmediate(resolve));
  await f.bridge.active.get(f.thread.id)?.finished;
  assert.equal(f.capturedOptions.at(-1).sessionId, result.sessionId);
  assert.equal(f.capturedOptions.at(-1).resume, undefined);
});

test("cold listing, repeated native attach and rewind preserve saved child ownership", async t => {
  const f = await rewindFixture(t), parent = f.sessions.get(f.thread.id);
  const child = await f.sessions.create(parent.options, { threadId: parent.id, depth: 1, name: "Scout", role: "research" });
  f.sessions.maxBytes = 1; f.sessions.evict();
  assert.equal(f.sessions.values.size, 0);
  const listed = await f.bridge.request("thread/list", {});
  assert.deepEqual(new Set(listed.data.map(s => s.id)), new Set([parent.id, child.id]));
  const params = { sessionId: f.id, model: "claude:haiku", wonderPolicy: f.policy, wonderProject: f.project };
  const attached = await Promise.all([f.bridge.request("project/session/attach", params), f.bridge.request("project/session/attach", params)]);
  assert.deepEqual(attached.map(result => result.thread.id), [parent.id, parent.id]);
  assert.equal(f.sessions.ids.size, 2, "attaching cannot invent a second parent journal");
  await f.bridge.request("thread/rewind", { threadId: parent.id, turnId: "turn-1" });
  assert.equal(await f.sessions.using(child.id, session => session.parent.nativeSessionId), f.id,
    "a child evicted before rewind still records the old native parent");
  assert.deepEqual((await f.bridge.request("thread/list", {})).data.map(s => s.id), [parent.id]);
  assert.equal(f.inputs.length, 0);
});

test("history pages and projection revisions survive byte eviction without losing items", async t => {
  const f = await rewindFixture(t);
  f.bridge.sessionFiles.stamp = async () => "fixture-revision";
  f.bridge.transcripts.maxBytes = 1; f.bridge.transcripts.oversizeMs = 0; f.bridge.taskProjections.maxBytes = 1;
  const params = { threadId: f.thread.id, itemsView: "notLoaded", includeHistoryRevision: true, limit: 1 };
  const first = await f.bridge.request("thread/turns/list", params);
  const repeat = await f.bridge.request("thread/turns/list", params);
  assert.equal(first.historyRevision, repeat.historyRevision);
  assert.equal(first.nextCursor, repeat.nextCursor);
  const second = await f.bridge.request("thread/turns/list", { ...params, cursor: first.nextCursor });
  assert.deepEqual([first.data[0].id, second.data[0].id], ["turn-3", "turn-2"]);
  assert.equal(first.historyRevision, second.historyRevision);
  const items = await f.bridge.request("thread/items/list", { threadId: f.thread.id, sortDirection: "asc" });
  assert.equal(items.data.length, 6);
  assert.equal(f.bridge.transcripts.size, 0);
  assert.equal(f.bridge.taskProjections.size, 0);
});

test("an SDK activation fences an older native read before it can refill history caches", async t => {
  const f = await rewindFixture(t), native = f.bridge.withSdk.bind(f.bridge);
  f.bridge.sessionFiles.stamp = async () => "fixture-revision";
  const entered = Promise.withResolvers(), release = Promise.withResolvers();
  f.bridge.withSdk = async action => { entered.resolve(); await release.promise; return native(action); };
  const reading = f.bridge.request("thread/turns/list", { threadId: f.thread.id });
  const rejected = assert.rejects(reading, /updating this conversation/);
  await entered.promise;
  await f.bridge.runtimeActivated({ version: "changed-fixture-sdk" });
  release.resolve(); await rejected;
  assert.equal(f.bridge.transcripts.size, 0);
  f.bridge.withSdk = native;
  assert.deepEqual((await f.bridge.request("thread/turns/list", { threadId: f.thread.id })).data.map(t => t.id), ["turn-3", "turn-2", "turn-1"]);
});

// Contract: with the owner's backups on, each reply's process keeps file
// checkpoints and the turn records it; restoring files on rewind needs every
// removed reply to have run that way, runs a dry run first in a process that
// sends no prompt, and leaves everything unchanged when Claude can't restore.
test("restoring files on rewind needs backups for every removed reply and checks first", async t => {
  const claudeSessionsDir = await mkdtemp(join(tmpdir(), "wonder-claude-sessions-"));
  t.after(() => rm(claudeSessionsDir, { recursive: true, force: true }));
  const transcripts = {};
  const f = await fixture(t, { transcripts, claudeSessionsDir });
  const { thread } = await f.bridge.request("thread/start", { cwd: f.primary, model: "claude:haiku", wonderPolicy: f.policy, wonderProject: f.project });
  const send = async (text, checkpointing) => {
    const { turn } = await f.bridge.request("turn/start", { threadId: thread.id, input: [{ type: "text", text }],
      ...(checkpointing ? { fileCheckpointing: true } : {}) });
    await new Promise(resolve => setImmediate(resolve));
    await f.bridge.active.get(thread.id)?.finished;
    return turn.id;
  };
  const first = await send("Edit a.txt", true);
  assert.equal(f.capturedOptions.at(-1).enableFileCheckpointing, true);
  const second = await send("Edit it again", true);
  const third = await send("And once more", false);
  const session = f.sessions.get(thread.id);
  assert.deepEqual(session.turns.map(turn => turn.checkpointed === true), [true, true, false]);
  transcripts[session.sdkSessionId] = { cwd: f.primary, messages: [first, second, third].flatMap((id, i) => [
    { type: "user", uuid: id, origin: { kind: "human" }, message: { content: `Ask ${i}` } },
    { type: "assistant", uuid: `reply-${i}`, message: { id: `m${i}`, content: [{ type: "text", text: "Edited" }] } }]) };

  await assert.rejects(f.bridge.request("thread/rewind", { threadId: thread.id, turnId: second, restoreFiles: true }), /didn't keep file backups/);
  assert.equal(f.rewinds.length, 0);
  f.bridge.sessions.get(thread.id).turns[2].checkpointed = true;

  const prompts = f.inputs.length;
  f.rewindFiles.current = () => ({ canRewind: false, error: "No file checkpoint found for this message." });
  await assert.rejects(f.bridge.request("thread/rewind", { threadId: thread.id, turnId: second, restoreFiles: true }), /can't restore the files/);
  assert.deepEqual(f.rewinds.map(r => r.opts.dryRun === true), [true], "nothing restored after a failed check");
  assert.equal(f.sessions.get(thread.id).rewind, undefined, "the conversation is unchanged");

  f.rewinds.length = 0;
  f.rewindFiles.current = (_id, { dryRun = false } = {}) => ({ canRewind: true, ...(dryRun ? {} : { filesChanged: ["a.txt", "b.txt"], skippedLinks: 1 }) });
  const result = await f.bridge.request("thread/rewind", { threadId: thread.id, turnId: second, restoreFiles: true });
  assert.deepEqual(result, { removedTurnIds: [second, third], filesRestored: true, filesChanged: 2, filesSkipped: 1 });
  assert.deepEqual(f.rewinds.map(r => [r.id, r.opts.dryRun === true]), [[second, true], [second, false]]);
  const control = f.rewinds[0].options;
  assert.equal(control.resume, session.sdkSessionId);
  assert.equal(control.enableFileCheckpointing, true);
  assert.equal(control.persistSession, false);
  assert.deepEqual(control.settingSources, [], "no hooks or settings run for a restore");
  assert.equal(f.inputs.length, prompts, "restoring files sends Claude no prompt");
  assert.deepEqual(f.sessions.get(thread.id).rewind, { turnId: second, at: "reply-0" });
  assert.deepEqual(f.sessions.get(thread.id).turns.map(turn => turn.id), [first]);
});

// A background command started in one turn must settle wherever its end
// arrives: mid-turn, while Claude is idle, after a kill in a later turn, when
// the process exits, and from the transcript after a resume. Synthetic SDK
// shapes (SDKTaskNotificationMessage; Bash's backgroundTaskId result).
const bgLaunch = [
  { type: "assistant", uuid: "bg-a", message: { id: "bg-m", content: [{ type: "tool_use", id: "tu-bg", name: "Bash", input: { command: "make watch", run_in_background: true } }] } },
  { type: "user", uuid: "bg-r", message: { content: [{ type: "tool_result", tool_use_id: "tu-bg", content: "Command running in background with ID: bgtask. Output is being written to: /tmp/bgtask.output" }] },
    tool_use_result: { stdout: "", stderr: "", interrupted: false, backgroundTaskId: "bgtask" } }];
const bgNotice = (status = "completed", extra = {}) => ({ type: "system", subtype: "task_notification", task_id: "bgtask", tool_use_id: "tu-bg", status, output_file: "/tmp/bgtask.output", summary: `Background command "make watch" ${status}`, ...extra });
async function backgroundCommand(t, { during = [], finish = true } = {}) {
  const f = await fixture(t);
  f.lingers.current = true;
  const { thread } = await f.bridge.request("thread/start", { cwd: f.primary, model: "claude:haiku", wonderPolicy: f.policy, wonderProject: f.project });
  f.gate.current = Promise.withResolvers();
  const { turn } = await f.bridge.request("turn/start", { threadId: thread.id, input: [{ type: "text", text: "Watch the build" }] });
  await settle(() => f.bridge.active.get(thread.id)?.query);
  const run = f.bridge.active.get(thread.id);
  for (const message of [...bgLaunch, ...during]) f.emits.push(message);
  f.emits.push({ type: "system", subtype: "background_tasks_changed", tasks: [{ task_id: "bgtask", task_type: "local_bash", description: "make watch" }] });
  await settle(() => run.roster.tasks?.size);
  if (finish) { f.gate.current.resolve(); await run.finished; }
  const completions = () => f.frames.filter(frame => frame.method === "item/completed" && frame.params.item?.id === "tu-bg").map(frame => frame.params);
  const saved = async () => (await (await new Sessions(f.sessions.root).initialize()).load(thread.id)).turns.find(x => x.id === turn.id).items.find(i => i.id === "tu-bg");
  return { f, thread, turn, run, completions, saved };
}

test("a background command settles when its notice arrives mid-turn", async t => {
  const { f, run, completions, thread, turn } = await backgroundCommand(t, { during: [bgNotice("failed")], finish: false });
  try {
    await settle(() => completions().length);
    f.gate.current.resolve(); await run.finished;
    assert.deepEqual(completions().map(c => [c.item.status, c.turnId]), [["failed", turn.id]]);
    const done = f.frames.findLast(frame => frame.method === "turn/completed").params.turn;
    assert.equal(done.items.find(i => i.id === "tu-bg").status, "failed");
    assert.equal(f.sessions.get(thread.id).turns.at(-1).items.find(i => i.id === "tu-bg").status, "failed");
  } finally { await f.bridge.close(); }
});

test("a background command left running by its turn settles when Claude is idle", async t => {
  const { f, thread, turn, completions, saved } = await backgroundCommand(t);
  try {
    assert.equal(completions().length, 0, "the finished turn did not fail or complete it");
    assert.equal((await saved()).status, "inProgress");
    assert.ok(f.bridge.lingering.has(thread.id));
    f.emits.push(bgNotice("completed"));
    await settle(() => completions().length);
    f.emits.push(bgNotice("completed")); f.emits.push({ type: "system", subtype: "task_updated", task_id: "bgtask", patch: { status: "completed" } });
    f.emits.push({ type: "system", subtype: "background_tasks_changed", tasks: [] });
    await settle(() => f.bridge.lingering.get(thread.id)?.roster.tasks?.size === 0);
    assert.deepEqual(completions().map(c => [c.item.type, c.item.status, c.turnId]), [["commandExecution", "completed", turn.id]]);
    assert.equal(f.frames.filter(frame => frame.method === "item/started" && frame.params.item?.id === "tu-bg").length, 1);
    assert.equal((await saved()).status, "completed", "the durable turn no longer shows it running");
  } finally { await f.bridge.close(); }
});

test("killing a background command in a later turn settles the original card", async t => {
  const { f, thread, turn, completions } = await backgroundCommand(t);
  try {
    f.gate.current = Promise.withResolvers();
    const second = await f.bridge.request("turn/start", { threadId: thread.id, input: [{ type: "text", text: "Stop the watcher" }] });
    await settle(() => f.bridge.active.get(thread.id)?.query);
    const run = f.bridge.active.get(thread.id);
    f.emits.push({ type: "assistant", uuid: "k-a", message: { id: "k-m", content: [{ type: "tool_use", id: "tu-stop", name: "TaskStop", input: { task_id: "bgtask" } }] } });
    f.emits.push({ type: "user", uuid: "k-r", message: { content: [{ type: "tool_result", tool_use_id: "tu-stop", content: "Successfully stopped task: bgtask (make watch)" }] },
      tool_use_result: { message: "Successfully stopped task", task_id: "bgtask", task_type: "local_bash", command: "make watch" } });
    await settle(() => completions().length);
    f.gate.current.resolve(); await run.finished;
    assert.deepEqual(completions().map(c => [c.item.status, c.turnId]), [["interrupted", turn.id]]);
    assert.ok(f.frames.some(frame => frame.method === "item/completed" && frame.params.item?.id === "tu-stop" && frame.params.turnId === second.turn.id));
  } finally { await f.bridge.close(); }
});

test("a background command still running when its process ends is settled, not left running", async t => {
  for (const known of [false, true]) {
    const { f, thread, completions } = await backgroundCommand(t);
    try {
      // With a lookup the transcript's own outcome wins over a guess.
      f.bridge.projectMessages = f.bridge.projectTaskMessages = async () => ({ stamp: null, messages: [
        { type: "user", uuid: "turn-1", origin: { kind: "human" }, message: { content: "Watch" } },
        { ...bgLaunch[0], timestamp: new Date().toISOString() }, bgLaunch[1], ...(known ? [notice("tu-bg", "failed", "exited with code 2")] : [])], desktop: false });
      await f.bridge.endLinger(thread.id, known);
      assert.deepEqual(completions().map(c => c.item.status), [known ? "failed" : "interrupted"]);
    } finally { await f.bridge.close(); }
  }
});

test("a resumed conversation reads a background command's outcome from the transcript", async t => {
  const f = await fixture(t);
  const { thread } = await f.bridge.request("thread/start", { cwd: f.primary, model: "claude:haiku", wonderPolicy: f.policy, wonderProject: f.project });
  const session = f.sessions.get(thread.id);
  session.sdkStarted = true;
  const base = [{ type: "user", uuid: "turn-1", origin: { kind: "human" }, message: { content: "Watch" } }, bgLaunch[0], bgLaunch[1]];
  const statusOf = async (messages, desktopTasks = false) => {
    f.bridge.projectMessages = f.bridge.projectTaskMessages = async () => ({ stamp: null, messages, desktop: false, desktopTasks });
    return (await f.bridge.projectHistory(session)).turns[0].items.find(i => i.id === "tu-bg");
  };
  assert.equal((await statusOf([...base, notice("tu-bg", "completed", "done")])).status, "completed");
  assert.equal((await statusOf([...base, notice("tu-bg", "failed", "exit code 3")])).status, "failed");
  assert.equal((await statusOf(base)).status, "interrupted", "nothing runs it: its end went unreported");
  assert.equal((await statusOf(base, true)).status, "inProgress", "Claude on the Mac still runs it");
  // The host skips re-reading items while the turn list and its revision are
  // unchanged. The task list settles this item without a transcript write or
  // a turn status change, so the revision must still move.
  const unloaded = async desktopTasks => {
    f.bridge.projectMessages = f.bridge.projectTaskMessages = async () => ({ stamp: null, messages: base, desktop: false, desktopTasks });
    return f.bridge.request("thread/turns/list", { threadId: thread.id, itemsView: "notLoaded", limit: 100, includeHistoryRevision: true });
  };
  const [running, settled] = [await unloaded(true), await unloaded(false)];
  assert.deepEqual(running.data, settled.data, "the turn list alone cannot tell");
  assert.match(running.historyRevision, /^[0-9a-f]{16}$/);
  assert.notEqual(running.historyRevision, settled.historyRevision);
  assert.equal((await unloaded(false)).historyRevision, settled.historyRevision, "stable while nothing changes");
  await f.bridge.close();
});

test("history settles a background command by later reads and kills, and never twice", () => {
  const history = (...entries) => nativeTurns([{ type: "user", uuid: "turn-1", origin: { kind: "human" }, message: { content: "Go" } }, ...bgLaunch, ...entries])[0].items.find(i => i.id === "tu-bg");
  const call = (id, name, input) => ({ type: "assistant", uuid: `${id}-a`, message: { id: `${id}-m`, content: [{ type: "tool_use", id, name, input }] } });
  const result = (id, content, structured) => ({ type: "user", uuid: `${id}-r`, message: { content: [{ type: "tool_result", tool_use_id: id, content }] }, tool_use_result: structured });
  const killed = history(call("k", "TaskStop", { task_id: "bgtask" }), result("k", "stopped", { task_id: "bgtask" }));
  assert.equal(killed.status, "interrupted");
  // An earlier notice is final; a later kill cannot change it.
  const finished = history(notice("tu-bg", "completed", "done"), call("k", "TaskStop", { task_id: "bgtask" }), result("k", "stopped", { task_id: "bgtask" }));
  assert.equal(finished.status, "completed");
  const read = history(call("o", "TaskOutput", { task_id: "bgtask" }), result("o", "ok", { retrieval_status: "success", task: { task_id: "bgtask", status: "failed", exitCode: 4, output: "boom" } }));
  assert.deepEqual([read.status, read.exitCode, read.aggregatedOutput], ["failed", 4, "boom"]);
  assert.equal(history().status, "interrupted");
});

test("closing the runtime during a reply settles the background command it launched", async t => {
  const { f, completions, turn } = await backgroundCommand(t, { finish: false });
  await f.bridge.close();
  assert.deepEqual(completions().map(c => [c.item.status, c.turnId]), [["interrupted", turn.id]]);
  const done = f.frames.findLast(frame => frame.method === "turn/completed")?.params.turn;
  assert.notEqual(done?.items.find(i => i.id === "tu-bg")?.status, "inProgress");
});

test("a same-chunk request burst refuses only the excess IDs and keeps serving control responses", async () => {
  const received = [], sent = [], held = [];
  const receive = frame => {
    received.push(frame);
    if (!frame.method) return Promise.resolve();
    return new Promise(resolve => held.push(resolve));
  };
  const lines = [
    ...Array.from({ length: 130 }, (_, i) => ({ id: i + 1, method: "thread/read", params: { threadId: `t${i}` } })),
    { id: "approval-1", result: { decision: "accept" } },
    { method: "initialized", params: {} },
    { id: "approval-2", error: { code: 1, message: "declined" } },
  ].map(frame => JSON.stringify(frame)).join("\n") + "\n";
  const requests = await serveFrames([Buffer.from(lines)], { receive, send: async frame => { sent.push(frame); } });
  assert.equal(received.filter(frame => frame.method === "thread/read").length, 128);
  assert.deepEqual(received.filter(frame => !frame.method || frame.id == null).map(frame => frame.id ?? frame.method),
    ["approval-1", "initialized", "approval-2"]);
  assert.deepEqual(sent, [129, 130].map(id => ({ id, error: { code: -32000, message: BUSY_MESSAGE } })));
  assert.equal(requests.size, 128);
  // A finished request frees its slot for the next one.
  held[0]();
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(requests.size, 127);
});

test("runtime framing keeps split UTF-8, several frames per chunk and a per-frame byte limit", () => {
  const framer = new LineFramer({ maxFrameBytes: 16 });
  const text = Buffer.from(`${JSON.stringify("é✓")}\n`);
  const split = text.indexOf(0xc3) + 1;
  assert.deepEqual([...framer.push(text.subarray(0, split))], []);
  assert.deepEqual([...framer.push(text.subarray(split))], ['"é✓"']);
  // Many individually valid frames in one chunk exceed the limit together.
  const batch = Array.from({ length: 8 }, (_, i) => `{"n":${i}}`).join("\n") + "\n\n";
  assert.deepEqual([...framer.push(batch)].map(line => JSON.parse(line).n), [0, 1, 2, 3, 4, 5, 6, 7]);
  // One unterminated frame over the limit is refused, even across chunks.
  assert.deepEqual([...framer.push("0123456789")], []);
  assert.throws(() => [...framer.push("0123456789")], /exceeded the allowed size/);
});

test("a large runtime frame arriving in small chunks is framed in linear time", () => {
  const frame = Buffer.from(`${JSON.stringify({ id: 1, method: "turn/start", params: { text: "x".repeat(30 * 1024 * 1024) } })}\n`);
  const framer = new LineFramer();
  const started = process.hrtime.bigint();
  const lines = [];
  for (let at = 0; at < frame.length; at += 64 * 1024) lines.push(...framer.push(frame.subarray(at, at + 64 * 1024)));
  const elapsedMs = Number(process.hrtime.bigint() - started) / 1e6;
  assert.equal(lines.length, 1);
  assert.equal(lines[0].length, frame.length - 1);
  // Rescanning the whole buffer per chunk took about 2.7 s for this frame.
  assert.ok(elapsedMs < 1000, `framing took ${elapsedMs} ms`);
});

// Refusals the phone shows use the product noun "chat", not "thread".
test("a missing Claude model choice asks for one in this chat", () => {
  assert.equal(selectedModel("claude:sonnet"), "sonnet");
  for (const value of [undefined, "sonnet", "claude:"])
    assert.throws(() => selectedModel(value), { message: "Choose a Claude model for this chat." });
});

// The SDK starts at the summary and omits system entries. The native reader
// must restore the boundary along with the older turns, including a cache hit.
test("native compact boundary survives SDK history and cached item pages", { timeout: 30_000 }, async t => {
  const f = await fixture(t);
  const id = "11111111-2222-4333-8444-555555555559", cwd = await realpath(f.primary);
  const config = join(f.root, "native"), files = await nativeFiles(t, config);
  const sdk = await import("@anthropic-ai/claude-agent-sdk");
  const folder = join(config, "projects", projectFolderName(cwd));
  await mkdir(folder, { recursive: true });
  const rows = [
    { type: "user", uuid: "u1", parentUuid: null, message: { role: "user", content: "Fixture question" } },
    { type: "assistant", uuid: "a1", parentUuid: "u1", message: { role: "assistant", id: "m1", content: [{ type: "text", text: "Before boundary" }] } },
    { type: "system", subtype: "compact_boundary", uuid: "boundary", parentUuid: null, logicalParentUuid: "a1", content: "PRIVATE SYNTHETIC BOUNDARY" },
    { type: "user", uuid: "summary", parentUuid: "boundary", isCompactSummary: true, message: { role: "user", content: "PRIVATE SYNTHETIC SUMMARY" } },
    { type: "assistant", uuid: "a2", parentUuid: "summary", message: { role: "assistant", id: "m2", stop_reason: "end_turn", content: [{ type: "text", text: "After boundary" }] } },
    { type: "user", uuid: "u2", parentUuid: "a2", message: { role: "user", content: "Continue" } },
  ];
  await writeFile(join(folder, `${id}.jsonl`), rows.map((row, i) => JSON.stringify({ sessionId: id, cwd, isSidechain: false,
    timestamp: `2026-10-27T00:00:0${i}Z`, ...row })).join("\n") + "\n");
  const bridge = new ClaudeBridge({ updates: { acquire: async () => ({ runtime: { sdk }, release: async () => {} }) },
    sessions: f.sessions, send: async () => {}, claudeSessionsDir: join(config, "sessions") });
  bridge.sessionFiles = files;
  const session = await f.sessions.create({ wonderProject: { ...f.project, cwd } }, null, { sdkSessionId: id, sdkStarted: true });
  for (let read = 0; read < 2; read++) {
    const { data } = await bridge.request("thread/items/list", { threadId: session.id, sortDirection: "asc" });
    const markers = data.filter(row => row.item.type === "contextCompaction");
    assert.equal(markers.length, 1);
    assert.equal(markers[0].item.status, "completed");
    assert.equal(markers[0].turnId, "u1");
    assert.deepEqual(data.map(row => row.item.type), ["userMessage", "agentMessage", "contextCompaction", "agentMessage", "userMessage"]);
    assert.equal(JSON.stringify(data).includes("PRIVATE"), false);
  }
});

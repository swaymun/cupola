import test from "node:test";
import assert from "node:assert/strict";
import { TurnProjection, questionRequest, questionAnswer, usageWindow, toolContent, claudeToolItem, applyClaudeToolResult, ClaudeTaskList } from "../projection.mjs";
import * as projectionApi from "../projection.mjs";
import { nativeTurns } from "../project-history.mjs";

// Contract owner: provider event normalization. A duplicated final reply or lost
// failed-tool state must be caught before these events reach durable ingestion.
function fixture(options = {}) {
  const events = [], children = [], usage = [];
  const projection = new TurnProjection({ threadId: "claude:thread", turnId: "turn", emit: e => events.push(e), onChild: m => children.push(m), onUsage: w => usage.push(w), ...options });
  projection.start();
  return { projection, events, children, usage };
}
test("stream deltas and repeated final reconcile one stable item", () => {
  const { projection: p, events } = fixture();
  const stream = event => p.accept({ type: "stream_event", event });
  stream({ type: "message_start", message: { id: "message" } });
  stream({ type: "content_block_start", index: 0, content_block: { type: "text", text: "" } });
  for (const text of ["Hello", " world"]) stream({ type: "content_block_delta", index: 0, delta: { type: "text_delta", text } });
  const final = { type: "assistant", message: { id: "message", content: [{ type: "text", text: "Hello world" }] } };
  p.accept(final); p.accept(final);
  p.accept({ type: "future_observability_event", arbitrary: true });
  p.accept({ type: "result", subtype: "success" });
  p.accept(final); p.finish("failed");
  const completed = events.filter(e => e.method === "item/completed");
  assert.equal(completed.length, 1); assert.equal(completed[0].params.item.text, "Hello world");
  assert.ok(events.filter(e => e.method === "item/agentMessage/delta").every(e => e.params.itemId === completed[0].params.item.id));
  assert.equal(events.filter(e => e.method === "turn/completed").length, 1);
});
// A turn's outcome cancels only unfinished actions, never confirmed tool results.
test("terminal turns preserve tool results and cancel only unfinished actions", () => {
  for (const [name, input, expectedType, expectedTool, expectedServer] of [
    ["mcp__wonder__wonder_thread_list", {}, "dynamicToolCall", "wonder_thread_list", undefined],
    ["mcp__Roblox_Studio__run_code", {}, "mcpToolCall", "mcp__Roblox_Studio__run_code", "Claude"],
    ["Bash", { command: "echo hello" }, "commandExecution", undefined, undefined],
    ["PowerShell", { command: "Write-Output hello" }, "commandExecution", undefined, undefined],
    ["Write", { file_path: "/tmp/example.txt", content: "hello" }, "fileChange", undefined, undefined],
  ]) {
    for (const [status, pendingStatus, pendingError] of [
      ["completed", "failed", "Action did not return a confirmed result."],
      ["interrupted", "interrupted", "Action was cancelled."],
      ["failed", "failed", "Action did not return a confirmed result."],
    ]) {
      const { projection: p, events } = fixture();
      p.accept({ type: "assistant", message: { content: ["pending", "failed", "completed"].map(id => ({ type: "tool_use", id, name, input })) } });
      p.accept({ type: "user", message: { content: [
        { type: "tool_result", tool_use_id: "failed", is_error: true, content: "Save rejected" },
        { type: "tool_result", tool_use_id: "completed", content: "Saved" },
      ] } });
      const confirmed = events.filter(e => e.method === "item/completed").map(e => e.params.item);
      if (status === "interrupted") p.finish(status);
      else p.accept({ type: "result", subtype: status === "completed" ? "success" : "error_during_execution", errors: ["Turn failed"] });
      p.finish("failed", "Late terminal error");
      const done = events.filter(e => e.method === "item/completed").map(e => e.params.item);
      assert.equal(done.length, 3, `${name}/${status}: each action finishes once`);
      assert.deepEqual(done.filter(i => i.id !== "pending"), confirmed);
      const failed = done.find(i => i.id === "failed"), completed = done.find(i => i.id === "completed"), pending = done.find(i => i.id === "pending");
      assert.equal(failed.type, expectedType); assert.equal(failed.tool, expectedTool); assert.equal(failed.server, expectedServer);
      assert.equal(failed.status, "failed"); assert.equal(failed.success, false);
      assert.equal(failed.error?.message ?? failed.aggregatedOutput, "Save rejected");
      assert.equal(completed.status, "completed"); assert.equal(completed.success, true); assert.equal(completed.error, undefined);
      assert.equal(pending.status, pendingStatus, `${name}/${status}`);
      assert.equal(pending.success, false); assert.equal(pending.error.message, pendingError);
      const turns = events.filter(e => e.method === "turn/completed");
      assert.equal(turns.length, 1); assert.equal(turns[0].params.turn.status, status);
      assert.deepEqual(turns[0].params.turn.error, status === "failed" ? { message: "Turn failed" } : null);
      assert.deepEqual(turns[0].params.turn.items, [pending, failed, completed]);
    }
  }
});
// The proposed plan is shaped like a Codex plan item so the conversation renders
// both alike; the refused tool call itself never becomes a failed row.
test("a proposed plan becomes one completed plan item, not a failed tool call", () => {
  const { projection: p, events } = fixture();
  const plan = "# Plan\n1. Add the migration\n2. Wire the route";
  p.accept({ type: "assistant", message: { id: "m", content: [{ type: "tool_use", id: "plan-call", name: "ExitPlanMode", input: { plan } },
    { type: "tool_use", id: "empty-call", name: "ExitPlanMode", input: { plan: "  " } }] } });
  p.accept({ type: "user", message: { content: [{ type: "tool_result", tool_use_id: "plan-call", is_error: true, content: "The owner reviews the plan in Cupola." },
    { type: "tool_result", tool_use_id: "empty-call", is_error: true, content: "The owner reviews the plan in Cupola." }] } });
  p.accept({ type: "result", subtype: "success" });
  assert.deepEqual(events.filter(e => e.method.startsWith("item/")).map(e => [e.method, e.params.item.type, e.params.item.status]),
    [["item/started", "plan", "inProgress"], ["item/completed", "plan", "completed"]]);
  const item = events.find(e => e.method === "item/completed").params.item;
  assert.deepEqual([item.id, item.text], ["plan-call", plan]);
  const turn = events.at(-1).params.turn;
  assert.equal(turn.status, "completed");
  assert.deepEqual(turn.items.map(i => i.type), ["plan"]);
});

test("child messages never leak into the parent reply", () => {
  const { projection: p, events, children } = fixture();
  p.accept({ type: "assistant", parent_tool_use_id: "agent-call", message: { id: "child", content: [{ type: "text", text: "Child work" }] } });
  assert.equal(children.length, 1); assert.equal(events.length, 1);
});
test("image bytes remain media for daemon attachment validation", () => {
  assert.deepEqual(toolContent([{ type: "image", source: { type: "base64", media_type: "image/png", data: "synthetic" } }]),
    [{ type: "inputImage", imageUrl: "data:image/png;base64,synthetic" }]);
});
test("question IDs preserve selection arrays and labels containing commas", () => {
  const input = { questions: [{ question: "Which?", header: "Pick", multiSelect: true, options: [{ label: "A, B" }, { label: "C" }] }] };
  const normalized = questionRequest(input, "request");
  const result = questionAnswer(input, normalized, { answers: { "request:0": { answers: ["A, B", "C"] } } });
  assert.deepEqual(result.answers, { "Which?": ["A, B", "C"] });
  assert.throws(() => questionAnswer(input, normalized, { answers: { "different:0": { answers: ["C"] } } }));
  assert.throws(() => questionRequest({ questions: [input.questions[0], input.questions[0]] }, "request"));
});
test("single questions reject multiple selections and accept custom text", () => {
  const input = { questions: [{ question: "Tone?", options: [{ label: "Short" }, { label: "Detailed" }] }] };
  const normalized = questionRequest(input, "request");
  assert.equal(questionAnswer(input, normalized, { answers: { "request:0": { answers: ["Friendly"] } } }).answers["Tone?"], "Friendly");
  const exact = "  Café, please.\nKeep the accents — thanks.  ";
  assert.equal(questionAnswer(input, normalized, { answers: { "request:0": { answers: [exact] } } }).answers["Tone?"], exact);
  assert.equal(normalized.questions[0].isOther, true);
  assert.equal(normalized.questions[0].isSecret, false);
  assert.equal(normalized._wonderQuestionIncomplete, undefined);
  for (const edit of [value => { value.metadata = { preview: "extra context" }; },
    value => { value.questions[0].markdown = "extra context"; },
    value => { value.questions[0].options[0].markdown = "extra context"; },
    value => { value.questions[0].header = "x".repeat(81); },
    value => { value.questions[0].multiSelect = "true"; },
    value => { value.questions[0].options[0].description = { text: "extra context" }; }]) {
    const richer = structuredClone(input); edit(richer);
    const request = questionRequest(richer, "request");
    assert.equal(request._wonderQuestionIncomplete, true);
    assert.equal(JSON.stringify(request).includes("extra context"), false);
  }
  assert.throws(() => questionAnswer(input, normalized, { answers: { "request:0": { answers: ["Short", "Detailed"] } } }));
});
test("usage distinguishes SDK fractions from endpoint percentages and unknown", () => {
  assert.equal(usageWindow("five_hour", { utilization: 0.32, resetsAt: 1900000000 }).remainingPercent, 68);
  assert.equal(usageWindow("seven_day", { utilization: 32, format: "percent" }).usedPercent, 32);
  assert.equal(usageWindow("five_hour", { utilization: NaN }), null);
  assert.equal(usageWindow("seven_day", {}), null);
  assert.equal(usageWindow("future_window", { utilization: 0 }), null);
});

// SDK connection/quota failures can use the success variant with is_error and
// a result string. Preserve the actual error instead of a generic failure.
test("error results preserve their reason and never become successful replies", () => {
  for (const message of [
    { type: "result", subtype: "success", is_error: true, result: "API Error: Cannot reach server" },
    { type: "result", subtype: "error_max_turns", errors: ["Reached maximum number of turns (64)"] },
  ]) {
    const { projection, events } = fixture();
    projection.accept(message);
    assert.equal(events.at(-1).params.turn.status, "failed");
    assert.equal(events.at(-1).params.turn.error.message, message.result ?? message.errors[0]);
    assert.equal(events.at(-1).params.turn.items.length, 0);
  }
});

test("a result for earlier queued work does not end the prompt's turn", () => {
  const events = [];
  const projection = new TurnProjection({ threadId: "t", turnId: "turn-1", promptUuid: "turn-1", emit: e => events.push(e) });
  // A resumed session first finishes a queued background-task notice.
  projection.accept({ type: "result", subtype: "success", is_error: false, user_message_uuid: "notice-1" });
  assert.equal(projection.terminal, false);
  projection.accept({ type: "result", subtype: "success", is_error: false, user_message_uuids: ["turn-1"] });
  assert.equal(projection.terminal, true);
  assert.equal(projection.result.status, "completed");
  // A notice turn Claude started itself names no prompt; it ends the turn
  // only after the reply to this prompt began.
  const plain = new TurnProjection({ threadId: "t", turnId: "turn-2", promptUuid: "turn-2", emit: () => {} });
  plain.accept({ type: "result", subtype: "success", is_error: false });
  assert.equal(plain.terminal, false);
  plain.accept({ type: "assistant", user_message_uuid: "turn-2", message: { id: "m", content: [{ type: "text", text: "Done." }] }, parent_tool_use_id: null });
  plain.accept({ type: "result", subtype: "success", is_error: false });
  assert.equal(plain.terminal, true);
});

// The projection owns terminal errors: startup failures have no prompt UUID.
test("a session-level error fails an unanswered Project prompt", () => {
  const projection = new TurnProjection({ threadId: "t", turnId: "turn", promptUuid: "turn", emit: () => {} });
  projection.accept({ type: "result", subtype: "error_during_execution", is_error: true, errors: ["Worker failed"] });
  assert.equal(projection.terminal, true);
  assert.equal(projection.result.status, "failed");
  assert.equal(projection.result.error.message, "Worker failed");
});
test("a reply reports its context measurement or explicitly clears an unknown window", () => {
  const { projection: p, events } = fixture();
  const usage = { input_tokens: 12, cache_read_input_tokens: 40_000, cache_creation_input_tokens: 2_000, output_tokens: 800 };
  // Earlier requests of the same turn count toward the result's totals, not the window.
  p.accept({ type: "assistant", message: { id: "a", model: "claude-sonnet", usage: { input_tokens: 9_000, output_tokens: 10 }, content: [] } });
  p.accept({ type: "assistant", message: { id: "b", model: "claude-sonnet", usage, content: [{ type: "text", text: "Done" }] } });
  p.accept({ type: "result", subtype: "success", modelUsage: {
    "claude-haiku": { contextWindow: 200_000 }, "claude-sonnet": { contextWindow: 1_000_000 } } });
  const reports = events.filter(e => e.method === "thread/tokenUsage/updated");
  assert.equal(reports.length, 1);
  assert.equal(reports[0].params.tokenUsage.last.totalTokens, 42_812);
  assert.equal(reports[0].params.tokenUsage.modelContextWindow, 1_000_000);
  assert.equal(reports[0].params.tokenUsage.model, "claude-sonnet", "names the model its window belongs to");
  assert.ok(events.indexOf(reports[0]) < events.findIndex(e => e.method === "turn/completed"));

  const absent = fixture();
  absent.projection.accept({ type: "result", subtype: "error_during_execution", errors: ["Unavailable"], modelUsage: {} });
  assert.equal(absent.events.filter(e => e.method === "thread/tokenUsage/updated").length, 0,
    "a startup failure reports no new usage and must not erase the previous measurement");
  const unknown = fixture();
  unknown.projection.accept({ type: "assistant", message: { id: "a", usage, content: [] } });
  unknown.projection.accept({ type: "result", subtype: "success" });
  assert.equal(unknown.events.find(e => e.method === "thread/tokenUsage/updated").params.tokenUsage, null);

  // Only native alias metadata can associate a differently keyed model.
  for (const [modelUsage, expected] of [
    [{ "provider/sonnet": { canonicalModel: "claude-sonnet", contextWindow: 200_000 } }, 200_000],
    [{ "provider/sonnet": { canonicalModel: "claude-sonnet", contextWindow: 200_000 },
       "other/sonnet": { canonicalModel: "claude-sonnet", contextWindow: 1_000_000 } }, null],
    [{ "claude-sonnet": { contextWindow: 200_000 },
       "provider/sonnet": { canonicalModel: "claude-sonnet", contextWindow: 1_000_000 } }, 200_000],
  ]) {
    const mapped = fixture();
    mapped.projection.accept({ type: "assistant", message: { id: "a", model: "claude-sonnet", usage, content: [] } });
    mapped.projection.accept({ type: "result", subtype: "success", modelUsage });
    const report = mapped.events.find(e => e.method === "thread/tokenUsage/updated");
    assert.equal(report?.params.tokenUsage?.modelContextWindow ?? null, expected);
  }

  // A fallback/subagent model's window is not evidence for this reply's model.
  for (const model of ["unreported-model", undefined]) {
    const mismatch = fixture();
    mismatch.projection.accept({ type: "assistant", message: { id: "a", model, usage, content: [] } });
    mismatch.projection.accept({ type: "result", subtype: "success", modelUsage: {
      "claude-haiku": { contextWindow: 200_000 }, "claude-sonnet": { contextWindow: 1_000_000 }
    } });
    assert.equal(mismatch.events.find(e => e.method === "thread/tokenUsage/updated").params.tokenUsage, null,
      "an unmatched model must not borrow another model's window");
  }
});

test("native compaction invalidates context usage until a new measured reply", () => {
  const { projection, events } = fixture();
  projection.accept({ type: "assistant", message: { id: "before", model: "claude-sonnet", usage: { input_tokens: 12_000, output_tokens: 30 }, content: [] } });
  projection.accept({ type: "system", subtype: "compact_boundary", compact_metadata: { trigger: "auto", pre_tokens: 12_030 } });
  assert.equal(events.filter(e => e.method === "thread/tokenUsage/updated").at(-1)?.params.tokenUsage, null);
  projection.accept({ type: "result", subtype: "success", modelUsage: { "claude-sonnet": { contextWindow: 200_000 } } });
  assert.equal(events.filter(e => e.method === "thread/tokenUsage/updated").at(-1)?.params.tokenUsage, null,
    "a result cannot reuse the pre-compaction request");
  const next = fixture();
  next.projection.accept({ type: "system", subtype: "compact_boundary", compact_metadata: { trigger: "auto", pre_tokens: 12_030 } });
  next.projection.accept({ type: "assistant", message: { id: "after", model: "claude-sonnet", usage: { input_tokens: 1_200, output_tokens: 30 }, content: [] } });
  next.projection.accept({ type: "result", subtype: "success", modelUsage: { "claude-sonnet": { contextWindow: 200_000 } } });
  assert.equal(next.events.filter(e => e.method === "thread/tokenUsage/updated").at(-1).params.tokenUsage.last.totalTokens, 1_230);
});

// Built-in semantic inputs survive live/history projection without guessing from names.
test("native tool actions retain targets and confirmed outcomes", () => {
  const examples = [
    ["Read", { file_path: "/p/a.swift" }, { kind: "read", paths: ["/p/a.swift"] }],
    ["Grep", { pattern: "isWorking", path: "/p" }, { kind: "search", query: "isWorking", paths: ["/p"] }],
    ["Glob", { pattern: "**/*.swift" }, { kind: "listFiles", query: "**/*.swift" }],
    ["WebFetch", { url: "https://example.com" }, { kind: "fetch", url: "https://example.com" }],
    ["WebSearch", { query: "Swift" }, { kind: "webSearch", query: "Swift" }],
    ["TodoWrite", { todos: [] }, { kind: "updateTasks" }],
    ["TaskCreate", { subject: "Fix parser" }, { kind: "createTask" }],
    ["TaskUpdate", { taskId: "1" }, { kind: "updateTasks" }],
    ["TaskGet", { taskId: "1" }, { kind: "readTasks" }],
    ["TaskList", {}, { kind: "readTasks" }],
    ["TaskOutput", { task_id: "1" }, { kind: "readTasks" }],
    ["TaskStop", { task_id: "1" }, { kind: "stopTask" }],
    ["ToolSearch", { query: "+calendar" }, { kind: "findTools", query: "+calendar" }],
    ["AskUserQuestion", { questions: [] }, { kind: "ask" }],
    ["NotebookEdit", { notebook_path: "/p/a.ipynb" }, { kind: "edit", paths: ["/p/a.ipynb"] }],
  ];
  for (const [name, input, action] of examples) {
    const { projection: p, events } = fixture();
    const block = { type: "tool_use", id: "call", name, input };
    p.accept({ type: "assistant", message: { content: [block] } });
    assert.deepEqual(events.find(e => e.method === "item/started").params.item.toolAction, action, name);
    p.accept({ type: "user", message: { content: [{ type: "tool_result", tool_use_id: "call", is_error: true, content: "Rejected" }] } });
    const done = events.find(e => e.method === "item/completed").params.item;
    assert.deepEqual(done.toolAction, action); assert.equal(done.status, "failed");
    assert.deepEqual(claudeToolItem(block, "completed").toolAction, action);
  }
  assert.equal(claudeToolItem({ name: "mcp__arbitrary__read_all_secrets", input: {} }, "completed"), null);
  assert.deepEqual(claudeToolItem({ id: "b", name: "Bash", input: { command: "tool x", description: "Run tests" } }, "inProgress").toolAction,
    { kind: "command", description: "Run tests" });
});

// Only native thinking block boundaries establish a duration; neither content
// nor an unmatched turn result is evidence that thinking finished normally.
// The thinking text travels once, as the completed item's summary (one part
// per paragraph, like Codex's summary parts); the host redacts it and shows it
// only at the Thinking detail level. A running or interrupted block, a delta
// and redacted thinking never carry text.
test("thinking projects matched native lifecycle and the completed block's text", () => {
  const { projection: p, events } = fixture();
  const stream = event => p.accept({ type: "stream_event", event });
  stream({ type: "message_start", message: { id: "message" } });
  stream({ type: "content_block_start", index: 0, content_block: { type: "thinking", thinking: "" } });
  stream({ type: "content_block_delta", index: 0, delta: { type: "thinking_delta", thinking: "Checking the parser.\n\n" } });
  stream({ type: "content_block_delta", index: 0, delta: { type: "thinking_delta", thinking: "Then the tests." } });
  stream({ type: "content_block_delta", index: 0, delta: { type: "text_delta", text: "private malformed delta" } });
  stream({ type: "content_block_stop", index: 0 });
  stream({ type: "content_block_stop", index: 0 });
  stream({ type: "content_block_start", index: 1, content_block: { type: "thinking", thinking: "" } });
  stream({ type: "content_block_delta", index: 1, delta: { type: "thinking_delta", thinking: "unfinished private" } });
  p.finish("interrupted");
  const thinking = events.filter(e => e.params.item?.type === "reasoning");
  assert.deepEqual(thinking.map(e => [e.method, e.params.item.status]), [
    ["item/started", "inProgress"], ["item/completed", "completed"],
    ["item/started", "inProgress"], ["item/completed", "interrupted"],
  ]);
  assert.equal(thinking[0].params.item.id, thinking[1].params.item.id);
  assert.deepEqual(thinking[1].params.item.summary, ["Checking the parser.", "Then the tests."]);
  for (const event of [thinking[0], thinking[2], thinking[3]]) assert.equal(event.params.item.summary, undefined);
  assert.ok(!JSON.stringify(events).includes("private"));
  // An empty block is lifecycle only.
  const empty = fixture();
  empty.projection.stream({ type: "message_start", message: { id: "m" } });
  empty.projection.stream({ type: "content_block_start", index: 0, content_block: { type: "thinking", thinking: "" } });
  empty.projection.stream({ type: "content_block_delta", index: 0, delta: { type: "thinking_delta", thinking: " \n " } });
  empty.projection.stream({ type: "content_block_stop", index: 0 });
  // Redacted thinking is encrypted; it is lifecycle only too.
  empty.projection.stream({ type: "content_block_start", index: 1, content_block: { type: "redacted_thinking", data: "private redacted" } });
  empty.projection.stream({ type: "content_block_delta", index: 1, delta: { type: "thinking_delta", thinking: "private" } });
  empty.projection.stream({ type: "content_block_stop", index: 1 });
  assert.deepEqual(empty.events.filter(e => e.method === "item/completed").map(e => e.params.item),
    [{ type: "reasoning", id: "turn:m:thinking:0", status: "completed" }, { type: "reasoning", id: "turn:m:thinking:1", status: "completed" }]);
  assert.ok(!JSON.stringify(empty.events).includes("private"));
  const child = fixture();
  child.projection.accept({ type: "stream_event", parent_tool_use_id: "agent", event: { type: "content_block_start", index: 0, content_block: { type: "thinking" } } });
  assert.equal(child.events.length, 1);
  assert.equal(child.children.length, 1);
});

test("thinking ignores orphan boundaries and clears an unfinished message before the next one", () => {
  const { projection: p, events } = fixture();
  p.stream({ type: "message_start", message: { id: "m1" } });
  p.stream({ type: "content_block_stop", index: 0 });
  p.stream({ type: "content_block_start", index: 0, content_block: { type: "thinking" } });
  p.stream({ type: "content_block_start", index: 1, content_block: { type: "redacted_thinking", data: "private redacted" } });
  p.stream({ type: "message_start", message: { id: "m2" } });
  p.stream({ type: "content_block_stop", index: 1 });
  const items = events.filter(e => e.params.item?.type === "reasoning");
  assert.deepEqual(items.map(e => e.params.item.status), ["inProgress", "interrupted"]);
  assert.ok(!JSON.stringify(events).includes("private"));
});

// The native result owns whether a proposed TodoWrite took effect.
test("task lists apply only confirmed TodoWrite results and retain native before/after", () => {
  const before = [{ content: "Check parser", status: "pending", activeForm: "Checking parser" }];
  const after = [{ ...before[0], status: "completed" }, { content: "Review output", status: "future_status" }];
  for (const failed of [false, true]) {
    const item = claudeToolItem({ id: "todo", name: "TodoWrite", input: { todos: before } }, "inProgress");
    assert.equal(item.taskChecklist, undefined);
    applyClaudeToolResult(item, { is_error: failed, content: failed ? "Failed" : "Updated" }, { oldTodos: before, newTodos: after });
    if (failed) { assert.equal(item.taskChecklist, undefined); continue; }
    assert.equal(item.taskChecklist.completed, 1);
    assert.equal(item.taskChecklist.total, 2);
    assert.equal(item.taskChecklist.entries[1].status, "future_status");
    assert.equal(item.taskChecklist.history.length, 2);
  }
  const { projection: p } = fixture();
  p.startTool({ id: "unconfirmed", name: "TodoWrite", input: { todos: after } });
  p.finish("interrupted");
  assert.equal(p.result.items[0].taskChecklist, undefined);
  assert.equal(p.result.items[0].success, false);
});

// Claude Code's task list (TaskCreate/TaskUpdate) changes one task per call
// and belongs to the session. Shapes follow a recorded session: TaskCreate's
// result names the new ID ("Task #N created…", structured {task:{id}}), and
// TaskUpdate sends only the changed fields.
const taskCall = (id, name, input) => ({ type: "assistant", message: { id: `m-${id}`, content: [{ type: "tool_use", id, name, input }] } });
const taskResult = (id, text, result, isError = false) => ({ type: "user", tool_use_result: result,
  message: { content: [{ type: "tool_result", tool_use_id: id, content: text, ...(isError ? { is_error: true } : {}) }] } });
const prompt = (uuid, text) => ({ type: "user", uuid, message: { content: text } });
test("Claude's task list builds one checklist per turn across the session", () => {
  const messages = [
    prompt("turn-1", "Plan the work"),
    taskCall("c1", "TaskCreate", { subject: "Read the spec", description: "<text>" }),
    taskResult("c1", "Task #1 created successfully: Read the spec", { task: { id: "1", subject: "Read the spec" } }),
    taskCall("c2", "TaskCreate", { subject: "Run tests", description: "<text>", activeForm: "Running tests" }),
    taskResult("c2", "Task #2 created successfully: Run tests"),
    taskCall("u1", "TaskUpdate", { taskId: "1", status: "in_progress" }),
    taskResult("u1", "Updated task #1 status", { success: true, taskId: "1", updatedFields: ["status"] }),
    taskCall("u2", "TaskUpdate", { taskId: "1", status: "completed" }),
    taskResult("u2", "Updated task #1 status", { success: true, taskId: "1", updatedFields: ["status"] }),
    prompt("turn-2", "Continue"),
    taskCall("u3", "TaskUpdate", { taskId: "2", status: "in_progress" }),
    taskResult("u3", "Updated task #2 status", { success: true, taskId: "2", updatedFields: ["status"] }),
    taskCall("u4", "TaskUpdate", { taskId: "9", status: "completed" }),
    taskResult("u4", "Task not found", { success: false, taskId: "9", updatedFields: [], error: "Task not found" }),
  ];
  const turns = nativeTurns(messages);
  const cards = turns.map(turn => turn.items.filter(item => item.type === "taskChecklist"));
  assert.deepEqual(cards.map(list => list.map(item => item.id)), [["turn-1:tasks"], ["turn-2:tasks"]], "one card per turn that changed the list");
  const [first] = cards[0], [second] = cards[1];
  assert.deepEqual(first.taskChecklist.entries, [{ text: "Read the spec", status: "completed" }, { text: "Run tests", status: "pending" }]);
  assert.equal(first.taskChecklist.completed, 1);
  assert.equal(first.taskChecklist.history.length, 4, "each change in the turn is a version");
  // The second turn starts from the list the first turn left; a refused update changes nothing.
  assert.deepEqual(second.taskChecklist.entries, [{ text: "Read the spec", status: "completed" }, { text: "Running tests", status: "inProgress" }]);
  // The card follows its turn's first change; the calls stay in history for the record.
  const items = turns[0].items.map(item => item.id);
  assert.equal(items.indexOf("turn-1:tasks"), items.indexOf("c1") + 1);
});
test("a live reply continues the session's task list and updates its card in place", () => {
  const seeded = new ClaudeTaskList([{ id: "1", subject: "Read the spec", status: "completed" },
    { id: "2", subject: "Run tests", status: "pending", activeForm: "Running tests" }, { id: "3", subject: "Gone", status: "deleted" }]);
  const { projection: p, events } = fixture({ tasks: seeded });
  p.accept(taskCall("u1", "TaskUpdate", { taskId: "2", status: "in_progress" }));
  p.accept(taskResult("u1", "Updated task #2 status", { success: true, taskId: "2", updatedFields: ["status"] }));
  p.accept(taskCall("c3", "TaskCreate", { subject: "Ship it", description: "<text>" }));
  p.accept(taskResult("c3", "Task #3 created successfully: Ship it", { task: { id: "3", subject: "Ship it" } }));
  p.accept(taskCall("u2", "TaskUpdate", { taskId: "3", status: "deleted" }));
  p.accept(taskResult("u2", "Updated task #3 deleted", { success: true, taskId: "3", updatedFields: ["status"] }));
  p.accept(taskCall("bad", "TaskCreate", { subject: "Refused", description: "<text>" }));
  p.accept(taskResult("bad", "Error", null, true));
  const cards = events.filter(e => e.params.item?.id === "turn:tasks");
  assert.equal(events.filter(e => e.method === "item/started" && e.params.item?.id === "turn:tasks").length, 1, "one card");
  const latest = cards.at(-1).params.item;
  assert.equal(latest.type, "taskChecklist");
  assert.equal(latest.status, "completed");
  assert.deepEqual(latest.taskChecklist.entries, [{ text: "Read the spec", status: "completed" }, { text: "Running tests", status: "inProgress" }]);
  assert.equal(latest.taskChecklist.total, 2);
  assert.equal(latest.taskChecklist.history.length, 3);
  assert.equal(cards.filter(e => e.method === "item/completed").length, 3, "the refused call changed nothing");
});

test("native structured context reports are used directly without parsing their text", () => {
  const { projection, events } = fixture();
  projection.accept({ type: "assistant", context_usage: { total_tokens: 42_000, raw_max_tokens: 180_000, model: "claude-native" },
    message: { id: "context", model: "<synthetic>", content: [{ type: "text", text: "Native context report" }] } });
  projection.accept({ type: "result", subtype: "success", modelUsage: {} });
  const reports = events.filter(e => e.method === "thread/tokenUsage/updated");
  assert.equal(reports.length, 1);
  assert.equal(reports[0].params.tokenUsage.last.totalTokens, 42_000);
  assert.equal(reports[0].params.tokenUsage.modelContextWindow, 180_000);
});


test("child init cannot change the root native session identity", () => {
  const sessions = [], children = [];
  const projection = new TurnProjection({ threadId: "root", turnId: "turn", emit: () => {},
    onSession: id => sessions.push(id), onChild: message => children.push(message) });
  projection.accept({ type: "system", subtype: "init", session_id: "child", parent_tool_use_id: "spawn" });
  assert.deepEqual(sessions, []);
  assert.equal(children.length, 1);
  projection.accept({ type: "system", subtype: "init", session_id: "root-native" });
  assert.deepEqual(sessions, ["root-native"]);
});

// Background shell commands. Synthetic fixtures from the Agent SDK's documented
// shapes (SDKTaskNotificationMessage, SDKTaskUpdatedMessage) and Claude Code's
// launch wording ("Command running in background with ID: ..."); not recorded.
// A launch result only acknowledges the start, so the item must stay in
// progress across the end of its turn until a notice, read or kill settles it.
let backgroundSequence = 0;
function background(command = "npm run dev") {
  const n = ++backgroundSequence, id = `toolu_bg${n}`, task = `bg${n}task`, thread = `claude:bg${n}`;
  const call = { type: "assistant", message: { id: `m${n}`, content: [{ type: "tool_use", id, name: "Bash", input: { command, run_in_background: true } }] } };
  const ack = { type: "user", message: { content: [{ type: "tool_result", tool_use_id: id,
    content: `Command running in background with ID: ${task}. Output is being written to: /tmp/${task}.output` }] },
    tool_use_result: { stdout: "", stderr: "", interrupted: false, backgroundTaskId: task } };
  return { id, task, thread, call, ack, turn: `turn${n}`,
    open: (turnId = `turn${n}`, extra = {}) => fixture({ threadId: thread, turnId, ...extra }) };
}
const notice = (b, status, summary = `Background command "x" ${status}`) =>
  ({ type: "system", subtype: "task_notification", task_id: b.task, tool_use_id: b.id, status, output_file: `/tmp/${b.task}.output`, summary });
const settledItems = events => events.filter(e => e.method === "item/completed").map(e => e.params);

test("a background command stays in progress after its launch result and past the end of its turn", () => {
  const b = background(), { projection: p, events } = b.open();
  p.accept(b.call); p.accept(b.ack);
  p.accept({ type: "result", subtype: "success" });
  assert.deepEqual(settledItems(events), [], "nothing completed or failed the running command");
  const turn = events.at(-1).params.turn;
  assert.equal(turn.items[0].status, "inProgress");
  assert.equal(turn.items[0].error, undefined);
  // The completion arrives after the turn, in the reply Claude writes for it.
  const { projection: follow, events: later } = b.open(b.turn, { continuation: true });
  follow.accept(notice(b, "completed"));
  const [done] = settledItems(later);
  assert.deepEqual([done.item.id, done.item.type, done.item.status, done.item.success, done.turnId], [b.id, "commandExecution", "completed", true, b.turn]);
  assert.equal(later.filter(e => e.method === "item/started").length, 0, "the same item, not a second one");
  follow.accept(notice(b, "completed"));
  assert.equal(settledItems(later).length, 1, "a repeated notice changes nothing");
});

test("a background command's failure and stop reach the original item with their own state", () => {
  for (const [status, expected] of [["failed", "failed"], ["stopped", "interrupted"]]) {
    const b = background(), { projection: p } = b.open();
    p.accept(b.call); p.accept(b.ack); p.accept({ type: "result", subtype: "success" });
    const { projection: later, events } = b.open("a-later-turn");
    later.accept(notice(b, status, 'Background command "build" failed with exit code 2'));
    const [done] = settledItems(events);
    assert.equal(done.turnId, b.turn, "published under the turn that started it");
    assert.equal(done.item.status, expected);
    if (status === "failed") { assert.equal(done.item.success, false); assert.match(done.item.error.message, /exit code 2/); }
    else assert.equal(done.item.error, undefined);
  }
});

test("a queued task-notification message and a task_updated patch settle the command too", () => {
  const queued = background(), a = queued.open();
  a.projection.accept(queued.call); a.projection.accept(queued.ack);
  a.projection.accept({ type: "user", message: { role: "user", content:
    `<task-notification>\n<task-id>${queued.task}</task-id>\n<tool-use-id>${queued.id}</tool-use-id>\n<status>completed</status>\n<summary>done</summary>\n</task-notification>` } });
  assert.deepEqual(settledItems(a.events).map(e => e.item.status), ["completed"]);
  const patched = background(), c = patched.open();
  c.projection.accept(patched.call); c.projection.accept(patched.ack);
  c.projection.accept({ type: "system", subtype: "task_updated", task_id: patched.task, patch: { status: "killed" } });
  assert.deepEqual(settledItems(c.events).map(e => e.item.status), ["interrupted"]);
});

test("a command moved to the background by a timeout is tracked like a launched one", () => {
  const id = "toolu_moved", { projection: p, events } = fixture({ threadId: "claude:moved", turnId: "turn-moved" });
  p.accept({ type: "assistant", message: { id: "mm", content: [{ type: "tool_use", id, name: "Bash", input: { command: "sleep 600" } }] } });
  p.accept({ type: "user", message: { content: [{ type: "tool_result", tool_use_id: id,
    content: "Command did not complete within its 120s timeout and was moved to the background (ID: bmoved1). Output is being written to: /tmp/bmoved1.output" }] } });
  p.accept({ type: "result", subtype: "success" });
  assert.equal(settledItems(events).length, 0);
  p.accept({ type: "system", subtype: "task_notification", task_id: "bmoved1", status: "completed", output_file: "", summary: "done" });
  assert.deepEqual(settledItems(events).map(e => [e.item.id, e.item.status]), [[id, "completed"]]);
});

test("a foreground command that only prints launch wording still completes", () => {
  const { projection: p, events } = fixture({ threadId: "claude:fg", turnId: "turn-fg" });
  p.accept({ type: "assistant", message: { id: "fg", content: [{ type: "tool_use", id: "toolu_fg", name: "Bash", input: { command: "echo hi" } }] } });
  p.accept({ type: "user", message: { content: [{ type: "tool_result", tool_use_id: "toolu_fg", content: "I said: Command running in background with ID: nope" }] } });
  assert.deepEqual(settledItems(events).map(e => e.item.status), ["completed"]);
});

test("BashOutput reads settle the command once it ended and leave a running one alone", () => {
  const b = background(), { projection: p } = b.open();
  p.accept(b.call); p.accept(b.ack); p.accept({ type: "result", subtype: "success" });
  const read = (id, structured, text) => {
    const { projection: next, events: out } = b.open(`read-${id}`);
    next.accept({ type: "assistant", message: { id: `r${id}`, content: [{ type: "tool_use", id: `toolu_read${id}`, name: "BashOutput", input: { bash_id: b.task } }] } });
    next.accept({ type: "user", message: { content: [{ type: "tool_result", tool_use_id: `toolu_read${id}`, content: text }] }, tool_use_result: structured });
    return settledItems(out);
  };
  const running = read(1, { shellId: b.task, status: "running", stdout: "listening" }, "<status>running</status>\n<stdout>listening</stdout>");
  assert.deepEqual(running.map(e => [e.item.tool, e.item.status]), [["BashOutput", "completed"]], "the read itself completes; the command keeps running");
  const ended = read(2, { shellId: b.task, status: "failed", exitCode: 3, stdout: "boom", stderr: "" }, "<status>failed</status>\n<exit_code>3</exit_code>");
  assert.deepEqual(ended.map(e => [e.item.id, e.item.status, e.turnId]), [["toolu_read2", "completed", "read-2"], [b.id, "failed", b.turn]]);
  assert.equal(ended[1].item.aggregatedOutput, "boom");
  assert.equal(ended[1].item.exitCode, 3);
});

test("KillShell ends the command as stopped; a refused kill does not", () => {
  const b = background(), { projection: p } = b.open();
  p.accept(b.call); p.accept(b.ack); p.accept({ type: "result", subtype: "success" });
  const kill = (id, block) => {
    const { projection: next, events } = b.open(`kill-${id}`);
    next.accept({ type: "assistant", message: { id: `k${id}`, content: [{ type: "tool_use", id: `toolu_kill${id}`, name: "KillShell", input: { shell_id: b.task } }] } });
    next.accept({ type: "user", message: { content: [{ type: "tool_result", tool_use_id: `toolu_kill${id}`, ...block }] } });
    return settledItems(events);
  };
  assert.deepEqual(kill(1, { is_error: true, content: "Shell is not running" }).map(e => e.item.status), ["failed"], "only the kill call failed");
  const done = kill(2, { content: `Successfully killed shell: ${b.task}` });
  assert.deepEqual(done.map(e => [e.item.id, e.item.status]), [["toolu_kill2", "completed"], [b.id, "interrupted"]]);
});

test("a monitor started in the background stays running until its notice", () => {
  const { projection: p, events } = fixture({ threadId: "claude:mon", turnId: "turn-mon" });
  p.accept({ type: "assistant", message: { id: "mon", content: [{ type: "tool_use", id: "toolu_mon", name: "Monitor", input: { command: "tail -f log", description: "Watch log" } }] } });
  p.accept({ type: "user", message: { content: [{ type: "tool_result", tool_use_id: "toolu_mon", content: "Monitor started (task mon7, timeout 300000ms)." }] } });
  p.accept({ type: "result", subtype: "success" });
  assert.equal(settledItems(events).length, 0);
  p.accept({ type: "system", subtype: "task_notification", task_id: "mon7", tool_use_id: "toolu_mon", status: "completed", output_file: "", summary: "Monitor ended" });
  assert.deepEqual(settledItems(events).map(e => [e.item.tool, e.item.status]), [["Monitor", "completed"]]);
});

test("a command still running when its process ends is settled on request, not left running", () => {
  const b = background(), { projection: p } = b.open();
  p.accept(b.call); p.accept(b.ack); p.accept({ type: "result", subtype: "success" });
  const { projection: owner, events } = b.open("shutdown");
  owner.settleAllBackground("interrupted");
  assert.deepEqual(settledItems(events).map(e => [e.item.id, e.item.status]), [[b.id, "interrupted"]]);
  assert.equal(owner.settleBackground(b.id, { status: "completed" }), false);
});

test("a notice with no pending launch, or from housekeeping, publishes nothing", () => {
  const { projection: p, events } = fixture({ threadId: "claude:none", turnId: "turn-none" });
  p.accept({ type: "system", subtype: "task_notification", task_id: "unknown", tool_use_id: "toolu_unknown", status: "completed", output_file: "", summary: "x" });
  p.accept({ type: "system", subtype: "task_notification", task_id: "amb", status: "completed", output_file: "", summary: "x", ambient: true });
  assert.equal(events.filter(e => e.method.startsWith("item/")).length, 0);
});

test("agent tasks project through the child lifecycle exactly once", () => {
  const { projection: p, events, children } = fixture({ threadId: "claude:agent", turnId: "turn-agent" });
  p.accept({ type: "assistant", message: { id: "ag", content: [{ type: "tool_use", id: "toolu_agent", name: "Agent", input: { description: "Research", prompt: "go", run_in_background: true } }] } });
  p.accept({ type: "system", subtype: "task_started", task_id: "a1", tool_use_id: "toolu_agent", task_type: "local_agent", description: "Research" });
  p.accept({ type: "user", message: { content: [{ type: "tool_result", tool_use_id: "toolu_agent", content: "Async agent launched successfully. agentId: a1" }] }, tool_use_result: { status: "async_launched", agentId: "a1" } });
  p.accept({ type: "system", subtype: "task_notification", task_id: "a1", tool_use_id: "toolu_agent", status: "completed", output_file: "", summary: "done" });
  assert.deepEqual(children.map(m => m.subtype ?? m.type), ["task_started", "agent_result", "task_notification"]);
  assert.equal(events.filter(e => e.method.startsWith("item/")).length, 0, "the bridge owns the one agent row");
});

test("text before an agent launch becomes commentary live, like text before any tool", () => {
  const { projection: p, events } = fixture({ threadId: "claude:agent-note", turnId: "turn-note" });
  p.accept({ type: "assistant", message: { id: "n1", content: [{ type: "text", text: "Starting a researcher." },
    { type: "tool_use", id: "toolu_agent", name: "Agent", input: { description: "Research", prompt: "go" } }] } });
  const restated = events.filter(e => e.method === "item/completed" && e.params.item.type === "agentMessage");
  assert.deepEqual(restated.map(e => [e.params.item.text, e.params.item.phase]),
    [["Starting a researcher.", undefined], ["Starting a researcher.", "commentary"]]);
});

test("replay settles a background command by the same rules as the live stream", () => {
  const b = background(), item = { type: "commandExecution", id: b.id, command: "npm run dev", status: "unknown" };
  const transcript = [b.ack, { type: "user", origin: { kind: "task-notification" }, message: { role: "user", content:
    `<task-notification>\n<task-id>${b.task}</task-id>\n<tool-use-id>${b.id}</tool-use-id>\n<status>failed</status>\n<summary>exit code 1</summary>\n</task-notification>` } }];
  for (const entry of transcript) {
    const block = entry.message.content?.[0];
    if (block?.type === "tool_result") applyClaudeToolResult(item, block, entry.tool_use_result, null, { holdBackground: true });
    const parsed = projectionApi.parseTaskNotice(entry.message.content);
    if (parsed) assert.equal(projectionApi.settleBackgroundItem(item, { status: parsed.status, error: parsed.summary }), true);
  }
  assert.deepEqual([item.status, item.success, item.backgroundTaskId, item.error.message], ["failed", false, b.task, "exit code 1"]);
});

// A resumed conversation shows the notice's state, not the acknowledged launch.
test("history replay of a finished background command ends in the notice's state", () => {
  const b = background();
  const messages = [
    { type: "user", uuid: "u1", message: { role: "user", content: "start the dev server" } },
    { type: "assistant", uuid: "a1", message: { id: "m1", content: [b.call.message.content[0]] } },
    { type: "user", uuid: "u2", message: b.ack.message, tool_use_result: b.ack.tool_use_result },
    { type: "user", uuid: "u3", origin: { kind: "task-notification" }, message: { role: "user", content:
      `<task-notification>\n<task-id>${b.task}</task-id>\n<tool-use-id>${b.id}</tool-use-id>\n<status>failed</status>\n<summary>exit code 1</summary>\n</task-notification>` } },
  ];
  const item = nativeTurns(messages)[0].items.find(i => i.id === b.id);
  assert.equal(item.status, "failed");
});

// The host stores every picture it finds in a tool item (result, contentItems
// and output), and the journal saves the item as projected, so a screenshot
// must appear once, live and in history. Text stays in `result`, where pull
// request discovery and the phone's result text read it.
test("a tool result's screenshot is kept once while its text stays in the result", () => {
  const png = "iVBORw0KGgo".repeat(64);
  const content = [{ type: "text", text: "Opened https://github.com/o/r/pull/7" },
    { type: "image", source: { type: "base64", media_type: "image/png", data: png } },
    { type: "resource_link", uri: "file:///tmp/shot.png", name: "shot.png" }, { type: "future_block", value: 1 }];
  const tool = { type: "tool_use", id: "tu-shot", name: "mcp__github__create_pull_request", input: {} };
  const { projection: p, events } = fixture();
  p.accept({ type: "assistant", message: { id: "m", content: [tool] } });
  p.accept({ type: "user", message: { content: [{ type: "tool_result", tool_use_id: "tu-shot", content }] } });
  const live = events.find(e => e.method === "item/completed" && e.params.item.id === "tu-shot").params.item;
  const history = nativeTurns([{ type: "user", uuid: "u", message: { content: "Look" } },
    { type: "assistant", message: { id: "m", content: [tool] } },
    { type: "user", message: { content: [{ type: "tool_result", tool_use_id: "tu-shot", content }] } }])[0].items.find(i => i.id === "tu-shot");
  for (const item of [live, history]) {
    assert.equal(JSON.stringify(item).split(png).length - 1, 1, "the picture's bytes appear once");
    assert.equal(JSON.stringify(item).split("file:///tmp/shot.png").length - 1, 1, "the file link appears once");
    assert.deepEqual(item.contentItems.map(c => c.type), ["inputText", "inputImage", "resource_link"]);
  }
  assert.deepEqual(live.result, { content: [content[0], content[3]] }, "text and unknown blocks stay in the result");
  assert.deepEqual(history.contentItems, live.contentItems);
  p.accept({ type: "assistant", message: { id: "m2", content: [{ ...tool, id: "tu-text" }] } });
  p.accept({ type: "user", message: { content: [{ type: "tool_result", tool_use_id: "tu-text", content: "https://github.com/o/r/pull/8" }] } });
  const plain = events.find(e => e.method === "item/completed" && e.params.item.id === "tu-text").params.item;
  assert.deepEqual(plain.result, { content: "https://github.com/o/r/pull/8" }, "a text-only result is unchanged");
});

// Live replay and a later transcript read use the same UUID, never the summary.
test("native compaction emits one private completed boundary with history identity", () => {
  const { projection, events } = fixture();
  const boundary = { type: "system", subtype: "compact_boundary", uuid: "boundary", timestamp: "2026-10-27T00:00:03Z", content: "PRIVATE SUMMARY" };
  projection.accept(boundary); projection.accept(boundary);
  const completed = events.filter(e => e.method === "item/completed" && e.params.item.type === "contextCompaction");
  assert.equal(completed.length, 1);
  assert.equal(completed[0].params.item.createdAt, undefined, "native items must share the ordered import clock");
  const history = nativeTurns([{ type: "user", uuid: "turn", message: { content: "Fixture" } }, boundary,
    { type: "user", uuid: "summary", isCompactSummary: true, message: { content: "PRIVATE SUMMARY" } }]);
  assert.deepEqual(completed[0].params.item, history[0].items[1]);
  assert.equal(JSON.stringify(completed).includes("PRIVATE"), false);
});

test("T3 Code's /compact message shows as the compaction divider, not a bubble", () => {
  const rows = [
    { type: "user", uuid: "q", message: { content: "Fixture question" } },
    { type: "assistant", uuid: "a", message: { id: "a", content: [{ type: "text", text: "Before" }] } },
    { type: "user", uuid: "cmd", message: { content: "/compact keep PRIVATE focus" } },
    { type: "system", subtype: "compact_boundary", uuid: "boundary", content: "PRIVATE" },
    { type: "user", uuid: "out", message: { content: "<local-command-stdout>PRIVATE</local-command-stdout>" } },
    { type: "assistant", uuid: "b", message: { id: "b", content: [{ type: "text", text: "After" }] } },
  ];
  const turns = nativeTurns(rows);
  const json = JSON.stringify(turns);
  assert.ok(!json.includes("PRIVATE") && !json.includes("/compact"), json);
  const items = turns.flatMap(t => t.items);
  assert.equal(items.filter(i => i.type === "userMessage").length, 1);
  assert.equal(items.filter(i => i.type === "contextCompaction").length, 1);
});

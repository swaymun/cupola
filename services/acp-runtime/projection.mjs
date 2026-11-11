// ACP session updates become Wonder's App Server turn items here. The daemon
// owns durable delivery and presentation; nothing here is shown verbatim
// except the agent's own words, commands, file changes and plan.
import { callArguments, sessionTool, wonderTool } from "./tool-server.mjs";

export const BRIDGE_PROTOCOL = 1;
export const FAMILY = "opencode";
const THOUGHT_LIMIT = 64 * 1024;

const text = content => content?.type === "text" && typeof content.text === "string" ? content.text : "";
const lines = value => String(value ?? "").replace(/\n$/, "").split("\n");
// ACP diffs carry whole old and new text; one hunk keeps the change readable.
export function hunk(before, after) {
  if (before == null) return String(after ?? "");
  return ["@@ @@", ...(before === "" ? [] : lines(before).map(l => `-${l}`)), ...(after === "" ? [] : lines(after).map(l => `+${l}`))].join("\n");
}
const outputText = content => (Array.isArray(content) ? content : [])
  .filter(block => block?.type === "content").map(block => text(block.content)).filter(Boolean).join("\n");
// Keep native image bytes for the daemon's verified attachment pipeline.
const outputImages = content => (Array.isArray(content) ? content : [])
  .filter(block => block?.type === "content" && block.content?.type === "image")
  .map(({ content }) => ({ type: "image", mimeType: content.mimeType, data: content.data }));
const filled = value => value && typeof value === "object" && Object.keys(value).length > 0;
const STATUS = { pending: "inProgress", in_progress: "inProgress", completed: "completed", failed: "failed" };
const bounded = (value, limit) => typeof value === "string" && value.trim() ? value.trim().slice(0, limit) : null;

// Errors the agent's code checker reported for the files an edit changed
// (OpenCode's `metadata.diagnostics`: LSP diagnostics by path, 0-based
// positions). Warnings and hints, other files and malformed entries are left
// out; what remains is bounded.
export function editDiagnostics(raw, changedPaths) {
  if (!raw || typeof raw !== "object" || Array.isArray(raw)) return [];
  const result = [];
  for (const path of new Set(changedPaths)) {
    const entries = Array.isArray(raw[path]) ? raw[path] : [];
    let count = 0;
    for (const entry of entries) {
      if (result.length >= 10 || count >= 3) break;
      const start = entry?.range?.start, message = bounded(entry?.message, 512);
      if (entry?.severity !== 1 || !message || !Number.isSafeInteger(start?.line) || !Number.isSafeInteger(start?.character)
        || start.line < 0 || start.character < 0) continue;
      result.push({ path, line: start.line + 1, column: start.character + 1, message });
      count += 1;
    }
  }
  return result;
}

// OpenCode's own to-do list, as ACP plan entries. A list that is not an array
// of entries is not a checklist.
export function todoEntries(todos) {
  if (!Array.isArray(todos) || !todos.every(t => t && typeof t === "object" && typeof t.content === "string")) return null;
  return todos.map(t => ({ content: t.content, status: typeof t.status === "string" ? t.status : "unknown" }));
}

// What one ACP tool call looks like as a Wonder item, from everything the
// agent has said about it so far.
export function toolItem(call, label = "OpenCode") {
  const status = STATUS[call.status] ?? "inProgress";
  const input = call.rawInput ?? {};
  const locations = Array.isArray(call.locations) ? call.locations : [];
  const images = outputImages(call.content);
  if (call.kind === "execute") {
    const command = typeof input.command === "string" && input.command.trim() ? input.command : call.title ?? "command";
    const output = call.rawOutput?.output ?? outputText(call.content);
    const exit = call.rawOutput?.metadata?.exit;
    return { type: "commandExecution", id: call.toolCallId, command, cwd: input.workdir ?? input.cwd ?? null, status,
      ...(images.length ? { contentItems: images } : {}),
      ...(status === "inProgress" && !output ? {} : { aggregatedOutput: typeof output === "string" ? output : "" }),
      ...(Number.isInteger(exit) ? { exitCode: exit } : {}), ...(status === "inProgress" ? {} : { success: status === "completed" }) };
  }
  const diffs = (call.content ?? []).filter(block => block?.type === "diff" && typeof block.path === "string");
  if (call.kind === "edit" || call.kind === "delete" || call.kind === "move" || diffs.length) {
    const changes = diffs.length
      ? diffs.map(d => ({ path: d.path, kind: { type: d.oldText == null ? "add" : "update" }, diff: hunk(d.oldText, d.newText) }))
      : typeof input.filePath === "string" && typeof input.content === "string"
        ? [{ path: input.filePath, kind: { type: "add" }, diff: input.content }]
        : typeof input.filePath === "string" && typeof input.oldString === "string" && typeof input.newString === "string"
          ? [{ path: input.filePath, kind: { type: "update" }, diff: hunk(input.oldString, input.newString) }]
          : locations.filter(l => typeof l?.path === "string").map(l => ({ path: l.path, kind: { type: "update" }, diff: "" }));
    const failed = status === "failed";
    // Kept apart from `error`: the edit itself succeeded.
    const diagnostics = status === "completed" ? editDiagnostics(call.rawOutput?.metadata?.diagnostics, changes.map(c => c.path)) : [];
    return { type: "fileChange", id: call.toolCallId, status, changes,
      ...(images.length ? { contentItems: images } : {}),
      ...(diagnostics.length ? { diagnostics, diagnosticsReporter: label } : {}),
      ...(failed ? { error: { message: outputText(call.content) || "The edit failed." } } : {}) };
  }
  const output = outputText(call.content);
  // OpenCode reports its sub-agent and to-do tools with a generic ACP kind
  // (`task` reaches ACP as thinking); their names say what they do.
  const name = call.name ?? call.title;
  const subAgent = name === "task" && typeof input.subagent_type === "string";
  const kind = subAgent ? "spawnAgent" : name === "todowrite" ? "updateTasks" : name === "todoread" ? "readTasks"
    : { read: "read", search: "search", fetch: "fetch", think: "think", switch_mode: "changeMode" }[call.kind];
  const paths = locations.filter(l => typeof l?.path === "string").slice(0, 32).map(l => l.path.slice(0, 2048));
  const query = input.pattern ?? input.query;
  const toolAction = kind ? { kind, ...(paths.length ? { paths } : {}),
    ...(locations.length > 32 ? { pathsTruncated: true } : {}),
    ...(typeof query === "string" ? { query: query.slice(0, 512) } : {}),
    ...(typeof input.url === "string" ? { url: input.url.slice(0, 2048) } : {}),
    ...(subAgent && bounded(input.description, 256) ? { description: bounded(input.description, 256) } : {}) } : null;
  // Wonder's own thread tools and computer use reach OpenCode and Cursor as
  // a per-session MCP server.
  const wonder = wonderTool(call.name ?? call.title), computer = wonder ? null : sessionTool(call.name ?? call.title);
  return { type: "mcpToolCall", id: call.toolCallId, server: wonder ? "Cupola" : computer ? "Computer use" : label,
    tool: (wonder ?? computer)?.tool ?? call.name ?? call.title ?? call.kind ?? "tool",
    arguments: callArguments(call), status, ...(toolAction ? { toolAction } : {}), ...(status === "inProgress" ? {} : { success: status === "completed",
      contentItems: [...(output ? [{ type: "inputText", text: output.slice(0, 64 * 1024) }] : []), ...images],
      ...(status === "failed" ? { error: { message: output || "The tool failed." } } : {}) }) };
}



export function contextUsage(update) {
  const used = Number(update?.used), size = Number(update?.size);
  if (!Number.isFinite(used) || used <= 0 || !Number.isFinite(size) || size <= 0) return null;
  const breakdown = { totalTokens: used, inputTokens: used, cachedInputTokens: 0, outputTokens: 0, reasoningOutputTokens: 0 };
  return { last: breakdown, total: breakdown, modelContextWindow: size };
}

export class TurnProjection {
  constructor({ threadId, turnId, emit, family = FAMILY, label = "OpenCode", now = () => Date.now() }) {
    Object.assign(this, { threadId, turnId, emit, family, label, now });
    this.items = new Map(); this.calls = new Map(); this.started = new Set();
    this.message = null; this.segments = 0; this.terminal = false; this.firstTokenAt = null;
    this.thought = null; this.thoughts = 0;
  }
  notify(method, params = {}) {
    this.emit({ method, params: { threadId: this.threadId, turnId: this.turnId, ...params, agentFamily: this.family } });
  }
  start() { this.startedAt = this.now(); this.notify("turn/started", { turn: { id: this.turnId, status: "inProgress", items: [] } }); }
  publish(item, refresh = false) {
    this.items.set(item.id, item);
    if (!this.started.has(item.id) || refresh) { this.started.add(item.id); this.notify("item/started", { item: { ...item } }); }
  }
  complete(item) {
    this.items.set(item.id, item);
    if (!this.started.has(item.id)) this.publish(item);
    this.notify("item/completed", { item: { ...item },
      ...(item.type === "taskChecklist" ? { notificationId: `${item.id}:${item.updatedAt}` } : {}) });
  }
  update(update) {
    if (this.terminal || !update) return;
    const kind = update.sessionUpdate;
    if (kind === "agent_thought_chunk") {
      this.firstTokenAt ??= this.now();
      const delta = text(update.content);
      if (!delta) return;
      // Thinking after text means more work follows, like a tool call.
      this.endMessage("commentary");
      if (!this.thought) {
        this.thoughts += 1;
        this.thought = { item: { type: "reasoning", id: `${this.turnId}:thinking:${this.thoughts}`, status: "inProgress" }, text: "" };
        this.publish(this.thought.item);
      }
      if (this.thought.text.length < THOUGHT_LIMIT) this.thought.text = (this.thought.text + delta).slice(0, THOUGHT_LIMIT);
      return;
    }
    if (["agent_message_chunk", "tool_call", "tool_call_update", "plan"].includes(kind)) this.endThought("completed");
    if (kind === "agent_message_chunk") {
      const delta = text(update.content);
      if (!delta) return;
      this.firstTokenAt ??= this.now();
      if (!this.message) {
        this.segments += 1;
        this.message = { type: "agentMessage", id: `${this.turnId}:message:${this.segments}`, text: "", status: "inProgress" };
        this.publish(this.message);
      }
      this.message.text += delta;
      this.notify("item/agentMessage/delta", { itemId: this.message.id, delta });
      return;
    }
    if (kind === "tool_call" || kind === "tool_call_update") {
      if (typeof update.toolCallId !== "string") return;
      // Text before a tool call was progress, like Codex commentary.
      this.endMessage("commentary");
      const previous = this.calls.get(update.toolCallId) ?? { toolCallId: update.toolCallId };
      // A late update cannot restart an already finished action.
      if (["completed", "failed"].includes(previous.status) && ["pending", "in_progress"].includes(update.status)) return;
      const call = { ...previous };
      for (const key of ["title", "kind", "status", "locations", "content", "rawOutput"]) if (update[key] != null) call[key] = update[key];
      if (filled(update.rawInput)) call.rawInput = update.rawInput;
      // The first title is the tool's name; later titles describe its target.
      call.name ??= update.title;
      this.calls.set(update.toolCallId, call);
      const item = toolItem(call, this.label);
      if (item.status === "completed" || item.status === "failed") this.complete(item);
      else this.publish(item, true);
      // Only a confirmed write changes the checklist; a failed one leaves it.
      const entries = call.name === "todowrite" && item.status === "completed"
        ? todoEntries(call.rawOutput?.metadata?.todos) ?? todoEntries(call.rawInput?.todos) : null;
      if (entries) this.plan(entries);
      return;
    }
    if (kind === "plan" && Array.isArray(update.entries)) { this.plan(update.entries); return; }
    if (kind === "usage_update") {
      const tokenUsage = contextUsage(update);
      if (tokenUsage) this.notify("thread/tokenUsage/updated", { tokenUsage });
    }
  }
  plan(entries) {
    const id = `${this.turnId}:tasks`;
    // Stamp the native receipt once, with deterministic order even when two
    // updates arrive in one millisecond. Saved history retains this stamp.
    const updatedAt = String(Math.max(this.now(), Number(this.items.get(id)?.updatedAt ?? 0) + 1));
    const plan = { type: "taskChecklist", id, updatedAt, status: "completed",
      taskChecklist: { entries: entries.slice(0, 32).map(e => ({ text: String(e.content ?? "").slice(0, 256),
        status: e.status === "in_progress" ? "inProgress" : String(e.status ?? "unknown").slice(0, 64) })),
        total: entries.length, completed: entries.filter(e => e.status === "completed").length,
        truncated: entries.length > 32 || entries.some(e => String(e.content ?? "").length > 256) } };
    this.complete(plan);
  }
  // The provider's own thinking text, sent once with the finished segment as
  // paragraphs in the shape of a reasoning summary. The host bounds, redacts
  // and shows it only at the Thinking detail level.
  endThought(status) {
    const thought = this.thought;
    if (!thought) return;
    this.thought = null;
    const summary = thought.text.split(/\n[ \t]*\n/).map(part => part.trim()).filter(Boolean).slice(0, 16);
    this.complete({ ...thought.item, status, ...(summary.length ? { summary } : {}) });
  }
  endMessage(phase) {
    const message = this.message;
    if (!message) return;
    this.message = null;
    message.status = "completed";
    if (phase) message.phase = phase;
    this.complete(message);
  }
  // `stopReason` from session/prompt, or an error that ended the prompt.
  finish({ stopReason = null, error = null } = {}) {
    if (this.terminal) return null;
    // A segment cut off by Stop or a failure is interrupted, not completed.
    this.endThought(stopReason && stopReason !== "cancelled" && !error ? "completed" : "interrupted");
    this.endMessage(null);
    this.terminal = true;
    for (const item of this.items.values()) {
      if (item.status !== "inProgress") continue;
      this.complete({ ...item, status: "failed", success: false, error: { message: "Action did not return a confirmed result." } });
    }
    const status = stopReason === "cancelled" ? "interrupted" : stopReason && !error ? "completed" : "failed";
    const reason = { max_tokens: `${this.label} reached the model's output limit.`, max_turn_requests: `${this.label} reached its limit of steps for one reply.`,
      refusal: "The model declined to answer." }[stopReason];
    const message = error ? String(error.message ?? error).slice(0, 2000) : reason ?? null;
    // The phone names the reason from the turn's error row, so a provider's own
    // text (its API refused the request) reaches it, not a bare "couldn't finish".
    // Sent as an item event first: the daemon ingests items from item/* only.
    if (status === "failed" && message) this.complete({ type: "error", id: `${this.turnId}:error`, text: message, message, status: "failed" });
    const items = [...this.items.values()];
    this.result = { id: this.turnId, status, items, error: message ? { message } : null };
    this.notify("turn/completed", { turn: this.result });
    return this.result;
  }
}

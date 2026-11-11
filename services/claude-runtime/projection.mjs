// Claude's wire messages stop here. The daemon owns durable delivery and presentation.
export const BRIDGE_PROTOCOL = 1;
export const HAIKU_MODEL = "claude-haiku-4-5-20251001";

export function toolContent(content) {
  if (typeof content === "string") return [{ type: "inputText", text: content }];
  if (!Array.isArray(content)) return [];
  return content.flatMap(block => {
    if (block.type === "text") return [{ type: "inputText", text: String(block.text ?? "") }];
    if (block.type === "image" && block.source?.type === "base64") {
      return [{ type: "inputImage", imageUrl: `data:${block.source.media_type};base64,${block.source.data}` }];
    }
    if (block.type === "image" && typeof block.data === "string") {
      return [{ type: "inputImage", imageUrl: `data:${block.mimeType};base64,${block.data}` }];
    }
    if (["resource", "resource_link"].includes(block.type)) return [block];
    return [];
  });
}
// A tool result's blocks without the pictures and files `toolContent` already
// carries. The host stores every picture it finds in an item, so each one goes
// in `contentItems` once; text stays here for readers of `result`.
function resultContent(content) {
  if (!Array.isArray(content)) return content ?? [];
  return content.filter(block => toolContent([block]).every(item => item.type === "inputText"));
}

export function questionRequest(input, requestId) {
  if (!Array.isArray(input?.questions) || input.questions.length < 1 || input.questions.length > 4) throw new Error("Invalid Claude question count");
  // Retain the fact that a native form has hidden context even when the phone's
  // existing form cannot display it. It must not become a simple notification.
  const incomplete = Object.keys(input).some(key => key !== "questions") || input.questions.some(q =>
    Object.keys(q).some(key => !["question", "header", "options", "multiSelect"].includes(key))
    || (q.header != null && (typeof q.header !== "string" || q.header.length > 80))
    || (q.multiSelect != null && typeof q.multiSelect !== "boolean")
    || (Array.isArray(q.options) && q.options.some(o => Object.keys(o).some(key => !["label", "description"].includes(key))
      || (o.description != null && typeof o.description !== "string"))));
  const seen = new Set();
  const questions = input.questions.map((q, i) => {
    if (typeof q.question !== "string" || !q.question.trim() || q.question.length > 4000 || seen.has(q.question)) throw new Error("Invalid or duplicate Claude question");
    seen.add(q.question);
    if (!Array.isArray(q.options) || q.options.length < 2 || q.options.length > 4) throw new Error("Invalid Claude question options");
    const labels = new Set();
    const options = q.options.map(o => {
      if (typeof o.label !== "string" || !o.label.trim() || o.label.length > 500 || labels.has(o.label)) throw new Error("Invalid or duplicate Claude option");
      labels.add(o.label);
      return { label: o.label, description: typeof o.description === "string" ? o.description.slice(0, 4000) : null };
    });
    return { id: `${requestId}:${i}`, header: typeof q.header === "string" ? q.header.slice(0, 80) : null,
      question: q.question, options, multiSelect: q.multiSelect === true, isOther: true, isSecret: false };
  });
  return { kind: "question", agentFamily: "claude", questions, isBlocking: true,
    ...(incomplete ? { _wonderQuestionIncomplete: true } : {}) };
}

export function questionAnswer(original, normalized, response) {
  const keys = Object.keys(response?.answers ?? {});
  if (keys.length !== normalized.questions.length || keys.some(k => !normalized.questions.some(q => q.id === k))) throw new Error("Answer does not match the pending question");
  const answers = {};
  for (const q of normalized.questions) {
    const values = response.answers[q.id]?.answers;
    if (!Array.isArray(values) || !values.length || values.length > 8 || (!q.multiSelect && values.length !== 1)
      || values.some(v => typeof v !== "string" || !v.trim() || v.length > 8000) || new Set(values).size !== values.length) throw new Error("Invalid question answer");
    // Current SDK accepts the documented array form; retain commas inside labels.
    answers[q.question] = q.multiSelect ? values : values[0];
  }
  return { ...original, answers };
}

// ExitPlanMode carries the proposed plan as markdown. The conversation shows it
// as a plan item shaped like Codex's ({ type, id, text }); a call without plan
// text has nothing to show and produces no row.
// Claude Code's shell and file tools use the same rows as Codex: a command
// with its output, or a file change with its patch.
const lines = text => String(text ?? "").replace(/\n$/, "").split("\n");
const hunk = (before, after) => ["@@ @@", ...(before === "" ? [] : lines(before).map(l => `-${l}`)),
  ...(after === "" ? [] : lines(after).map(l => `+${l}`))].join("\n");
export function claudeToolItem(block, status) {
  const input = block?.input ?? {};
  if (["Bash", "PowerShell"].includes(block?.name) && typeof input.command === "string" && input.command.trim()) {
    return { type: "commandExecution", id: block.id, command: input.command, status,
      toolAction: { kind: "command", ...(typeof input.description === "string" ? { description: input.description.slice(0, 512) } : {}) } };
  }
  const path = typeof input.file_path === "string" && input.file_path ? input.file_path : null;
  // Built-in inputs carry the action and target; never infer them from shell text.
  const kinds = { Read: "read", Glob: "listFiles", Grep: "search", WebFetch: "fetch", WebSearch: "webSearch",
    AskUserQuestion: "ask", ToolSearch: "findTools", TodoWrite: "updateTasks", TaskCreate: "createTask",
    TaskUpdate: "updateTasks", TaskGet: "readTasks", TaskList: "readTasks", TaskOutput: "readTasks",
    TaskStop: "stopTask", BashOutput: "readTasks", KillShell: "stopTask", NotebookEdit: "edit" };
  const kind = kinds[block?.name];
  if (kind) {
    const target = path ?? input.notebook_path ?? input.path;
    const query = input.pattern ?? input.query;
    return { type: "mcpToolCall", id: block.id, server: "Claude", tool: block.name, arguments: input, status,
      toolAction: { kind, ...(typeof target === "string" ? { paths: [target.slice(0, 2048)] } : {}),
        ...(typeof query === "string" ? { query: query.slice(0, 512) } : {}),
        ...(typeof input.url === "string" ? { url: input.url.slice(0, 2048) } : {}) } };
  }
  if (!path) return null;
  if (block.name === "Write" && typeof input.content === "string") {
    return { type: "fileChange", id: block.id, status, changes: [{ path, kind: { type: "add" }, diff: input.content }] };
  }
  if (block.name === "Edit" && typeof input.old_string === "string" && typeof input.new_string === "string") {
    return { type: "fileChange", id: block.id, status, changes: [{ path, kind: { type: "update" }, diff: hunk(input.old_string, input.new_string) }] };
  }
  if (block.name === "MultiEdit" && Array.isArray(input.edits)) {
    const edits = input.edits.filter(e => typeof e?.old_string === "string" && typeof e?.new_string === "string");
    if (edits.length) return { type: "fileChange", id: block.id, status,
      changes: [{ path, kind: { type: "update" }, diff: edits.map(e => hunk(e.old_string, e.new_string)).join("\n") }] };
  }
  return null;
}
// Claude Code's own record of a file edit: the hunks it applied against the
// file (`structuredPatch`), or a new file's content. Counting the tool input
// instead marks every old line removed and every new line added.
export function fileEditResult(result) {
  if (!result || typeof result !== "object" || typeof result.filePath !== "string") return null;
  if (result.type === "create" && typeof result.content === "string") return { filePath: result.filePath, type: "create", content: result.content };
  if (!Array.isArray(result.structuredPatch)) return null;
  const hunks = result.structuredPatch.filter(h => h && Array.isArray(h.lines) && [h.oldStart, h.oldLines, h.newStart, h.newLines].every(Number.isInteger));
  return { filePath: result.filePath, type: "update",
    structuredPatch: hunks.map(({ oldStart, oldLines, newStart, newLines, lines }) => ({ oldStart, oldLines, newStart, newLines, lines: lines.map(String) })) };
}
function applyFileEdit(item, result) {
  const edit = fileEditResult(result);
  const change = item.changes?.length === 1 ? item.changes[0] : null;
  if (!edit || !change || edit.filePath !== change.path) return;
  if (edit.type === "create") { change.kind = { type: "add" }; change.diff = edit.content; return; }
  change.kind = { type: "update" };
  change.diff = edit.structuredPatch.map(h => [`@@ -${h.oldStart},${h.oldLines} +${h.newStart},${h.newLines} @@`, ...h.lines].join("\n")).join("\n");
}
// Claude Code's task list (TaskCreate/TaskUpdate), which belongs to the
// session: each call changes one task, so the checklist is the list so far
// with that change applied. Seeded from Claude Code's own task files when a
// reply starts, or replayed from the transcript for history.
export class ClaudeTaskList {
  constructor(tasks = []) {
    this.tasks = new Map();
    for (const task of tasks) {
      if (task?.id == null || task.status === "deleted") continue;
      this.tasks.set(String(task.id), { subject: String(task.subject ?? ""), status: String(task.status ?? "pending"),
        activeForm: typeof task.activeForm === "string" ? task.activeForm : null });
    }
  }
  // Applies a successful TaskCreate/TaskUpdate; false when it changed nothing
  // Wonder can show (an unknown result shape, or a refused update).
  apply(tool, input = {}, result = null, text = "") {
    if (tool === "TaskCreate") {
      const id = result?.task?.id ?? /Task #(\S+) created/.exec(text)?.[1];
      if (id == null || typeof input.subject !== "string") return false;
      this.tasks.set(String(id), { subject: input.subject, status: "pending",
        activeForm: typeof input.activeForm === "string" ? input.activeForm : null });
      return true;
    }
    if (tool !== "TaskUpdate" || result?.success === false) return false;
    const id = String(input.taskId ?? result?.taskId ?? "");
    if (!id) return false;
    if (input.status === "deleted") return this.tasks.delete(id);
    const task = this.tasks.get(id) ?? { subject: `Task ${id}`, status: "pending", activeForm: null };
    if (typeof input.subject === "string") task.subject = input.subject;
    if (typeof input.activeForm === "string") task.activeForm = input.activeForm;
    if (typeof input.status === "string") task.status = input.status;
    this.tasks.set(id, task);
    return true;
  }
  // The shared checklist shape (crates/wonderd task_checklist.rs). A task in
  // progress reads as Claude Code shows it: its present-tense activeForm.
  snapshot() {
    const order = id => (/^\d+$/.test(id) ? Number(id) : Number.MAX_SAFE_INTEGER);
    const tasks = [...this.tasks].sort(([a], [b]) => order(a) - order(b) || a.localeCompare(b)).map(([, task]) => task);
    return {
      entries: tasks.slice(0, 32).map(task => {
        const status = task.status === "in_progress" ? "inProgress" : task.status.slice(0, 64);
        const text = status === "inProgress" && task.activeForm ? task.activeForm : task.subject;
        return { text: text.slice(0, 256), status };
      }),
      total: tasks.length, completed: tasks.filter(task => task.status === "completed").length,
      truncated: tasks.length > 32 || tasks.some(task => task.subject.length > 256)
    };
  }
}
export const CLAUDE_TASK_TOOLS = new Set(["TaskCreate", "TaskUpdate"]);
// The turn's checklist item for Claude's task list, updated in place: one
// card per turn that changes the list, with the turn's earlier versions.
export function taskChecklistItem(existing, turnId, latest, updatedAt = Date.now()) {
  const history = [...(existing?.taskChecklist?.history ?? (existing?.taskChecklist ? [existing.taskChecklist] : []))]
    .map(({ history: _, ...snapshot }) => snapshot);
  if (JSON.stringify(history.at(-1)) !== JSON.stringify(latest)) history.push(latest);
  return { type: "taskChecklist", id: `${turnId}:tasks`, status: "completed", updatedAt: String(updatedAt),
    taskChecklist: { ...latest, ...(history.length > 1 ? { history: history.slice(-8) } : {}) } };
}

// Background shell commands and monitors. Their tool result only acknowledges
// the launch; the outcome arrives later as a task notice, a BashOutput/
// TaskOutput read or a KillShell/TaskStop, possibly in another turn. A launched
// item stays in progress, keyed by its tool-use ID, until one of those settles
// it. The registry is process-wide because the settling event often reaches a
// different projection (a continuation, the next turn) than the one that
// started the item. The pure helpers are exported so transcript replay can
// settle history by the same rules.
const MAX_PENDING_BACKGROUND = 500;
const pendingBackground = new Map();
const TERMINAL = ["completed", "failed", "interrupted"];
const taskId = value => typeof value === "string" && /^[\w-]{1,64}$/.test(value) ? value : null;
// What a launch acknowledgement says, in Claude Code's wording. Anchored so a
// foreground command that merely prints these words is not taken for one.
export function backgroundLaunch(item, text, result) {
  if (item?.type === "commandExecution") {
    return taskId(result?.backgroundTaskId)
      ?? taskId(/^Command running in background with ID: ([\w-]{1,64})/.exec(text)?.[1])
      ?? taskId(/^Command[^\n]{0,300}?moved to the background \(ID: ([\w-]{1,64})\)/.exec(text)?.[1]);
  }
  if (item?.server === "Claude" && item.tool === "Monitor") return taskId(result?.taskId) ?? taskId(/^Monitor started \(task ([\w-]{1,64})/.exec(text)?.[1]);
  return null;
}
const outcomeStatus = status => ({ completed: "completed", success: "completed", failed: "failed", error: "failed",
  killed: "interrupted", stopped: "interrupted", cancelled: "interrupted" })[String(status ?? "").toLowerCase()] ?? null;
// Claude Code's queued completion notice, also written to the transcript:
// <task-notification><task-id>..</task-id><tool-use-id>..</tool-use-id><status>..
export function parseTaskNotice(text) {
  if (typeof text !== "string" || !text.includes("<task-notification>")) return null;
  const tag = name => text.match(new RegExp(`<${name}>([\\s\\S]*?)</${name}>`))?.[1]?.trim() || null;
  const notice = { taskId: tag("task-id"), toolUseId: tag("tool-use-id"), status: outcomeStatus(tag("status")), summary: tag("summary") };
  return notice.status && (notice.taskId || notice.toolUseId) ? notice : null;
}
// Moves a launched background item to its terminal state. False when it has
// already ended, so a repeated or late event changes nothing.
export function settleBackgroundItem(item, { status, error, output, exitCode } = {}) {
  if (!item || TERMINAL.includes(item.status) || !TERMINAL.includes(status)) return false;
  item.status = status;
  if (status === "completed") item.success = true;
  else if (status === "failed") { item.success = false; item.error = { message: String(error || "The background task failed.").slice(0, 2000) }; }
  else delete item.success;
  if (item.type === "commandExecution") {
    if (typeof output === "string") item.aggregatedOutput = output;
    if (Number.isInteger(exitCode)) item.exitCode = exitCode;
  }
  return true;
}
// What a BashOutput/TaskOutput read or a KillShell/TaskStop call says about the
// task it names: { targetId, ...outcome }, or null while the task still runs.
export function backgroundReadOutcome(item, text, result) {
  if (item?.server !== "Claude" || item.success !== true) return null;
  const input = item.arguments ?? {};
  const targetId = taskId(input.bash_id) ?? taskId(input.shell_id) ?? taskId(input.task_id);
  if (!targetId) return null;
  if (item.tool === "KillShell" || item.tool === "TaskStop") return { targetId, status: "interrupted" };
  if (item.tool !== "BashOutput" && item.tool !== "TaskOutput") return null;
  const source = result?.task && typeof result.task === "object" ? result.task : result ?? {};
  const status = outcomeStatus(source.status ?? /<status>\s*(\w+)\s*<\/status>/.exec(text)?.[1]);
  if (!status) return null;
  const code = [source.exitCode, source.exit_code].find(Number.isInteger) ?? (/<exit_code>\s*(-?\d+)/.exec(text)?.[1] ?? null);
  const output = typeof source.output === "string" ? source.output
    : [source.stdout, source.stderr].filter(v => typeof v === "string" && v).join("\n") || null;
  return { targetId, status, ...(output == null ? {} : { output }), ...(code == null ? {} : { exitCode: Number(code) }),
    ...(status === "failed" ? { error: `The background task failed${code == null ? "" : ` (exit code ${code})`}.` } : {}) };
}
function trackBackground(threadId, turnId, item) {
  pendingBackground.delete(item.id);
  pendingBackground.set(item.id, { threadId, turnId, item });
  while (pendingBackground.size > MAX_PENDING_BACKGROUND) pendingBackground.delete(pendingBackground.keys().next().value);
}
const isPendingBackground = item => pendingBackground.get(item.id)?.item === item;
function pendingFor({ toolUseId, taskId: task }) {
  const entry = toolUseId ? pendingBackground.get(toolUseId) : null;
  if (entry) return entry;
  return task ? [...pendingBackground.values()].find(e => e.item.backgroundTaskId === task) : null;
}

// Apply a tool result to a projected item, in place. `result` is the SDK's
// `tool_use_result` (Claude Code's `toolUseResult`) for the same message.
// `tasks` is the session's task list, changed by a successful TaskCreate or
// TaskUpdate; the return value says whether it changed. `holdBackground` keeps
// a background launch in progress because its outcome arrives later; history
// replay omits it and keeps the acknowledged state.
export function applyClaudeToolResult(item, block, result, tasks = null, { holdBackground = false } = {}) {
  item.success = block.is_error !== true;
  const contentItems = toolContent(block.content);
  const text = contentItems.filter(c => c.type === "inputText").map(c => c.text).join("\n");
  const launched = item.success ? backgroundLaunch(item, text, result) : null;
  if (launched) item.backgroundTaskId = launched;
  const holding = Boolean(launched && holdBackground);
  if (item.type === "commandExecution") {
    item.aggregatedOutput = text;
    item.status = holding ? "inProgress" : item.success ? "completed" : "failed";
    return;
  }
  if (item.type === "fileChange") {
    item.status = item.success ? "completed" : "failed";
    if (!item.success) item.error = { message: text || "Edit failed" };
    else applyFileEdit(item, result);
    return;
  }
  // The result confirms this native list was applied. Never present a failed
  // or unfinished TodoWrite input as an accepted task update.
  if (item.server === "Claude" && item.tool === "TodoWrite" && item.success) {
    const snapshot = todos => Array.isArray(todos) ? {
      entries: todos.slice(0, 32).map(todo => ({ text: String(todo.content ?? "").slice(0, 256),
        status: todo.status === "in_progress" ? "inProgress" : String(todo.status ?? "unknown").slice(0, 64) })),
      total: todos.length, completed: todos.filter(todo => todo.status === "completed").length,
      truncated: todos.length > 32 || todos.some(todo => String(todo.content ?? "").length > 256)
    } : null;
    const latest = snapshot(result?.newTodos ?? item.arguments?.todos);
    const previous = snapshot(result?.oldTodos);
    if (latest) item.taskChecklist = { ...latest,
      ...(previous && JSON.stringify(previous) !== JSON.stringify(latest) ? { history: [previous, latest] } : {}) };
  }
  const changedTasks = Boolean(tasks && item.server === "Claude" && CLAUDE_TASK_TOOLS.has(item.tool) && item.success
    && tasks.apply(item.tool, item.arguments, result, text));
  item.contentItems = contentItems;
  item.result = { content: resultContent(block.content) };
  item.status = holding ? "inProgress" : item.success ? "completed" : "failed";
  if (!item.success) item.error = { message: text || "Tool failed" };
  return changedTasks;
}

// Text written between tool calls is progress, like Codex commentary; only
// text after the turn's last tool call is the reply. Thinking is not a call.
// Returns changed items.
export function markCommentary(items) {
  const changed = [];
  let workAfter = false;
  for (let index = items.length - 1; index >= 0; index -= 1) {
    const item = items[index];
    if (item.type === "agentMessage") {
      if (workAfter && item.phase !== "commentary") { item.phase = "commentary"; changed.push(item); }
    } else if (!["userMessage", "plan", "reasoning"].includes(item.type)) workAfter = true;
  }
  return changed;
}

// A finished thinking block's text as summary parts, one per paragraph, in the
// shape of Codex's reasoning summary. The host redacts each part, bounds it and
// shows it only at the Thinking detail level; it keeps at most 16 parts.
export function thinkingSummary(text) {
  if (typeof text !== "string") return [];
  return text.split(/\n[ \t]*\n/).map(part => part.trim()).filter(Boolean).slice(0, 16);
}

export function planItem(block, status) {
  const text = typeof block?.input?.plan === "string" ? block.input.plan : "";
  return text.trim() ? { type: "plan", id: block.id, text, status } : null;
}

export function usageWindow(type, source, timestamp = Date.now()) {
  const definitions = { five_hour: ["5-hour limit", 300], seven_day: ["Weekly limit", 10080],
    seven_day_sonnet: ["Weekly · Sonnet", 10080], seven_day_opus: ["Weekly · Opus", 10080] };
  const definition = definitions[type];
  if (!definition || !source || typeof source.utilization !== "number" || !Number.isFinite(source.utilization)) return null;
  // SDK event utilization is a fraction; the account endpoint uses a percentage.
  const usedPercent = Math.max(0, Math.min(100, source.utilization * (source.format === "percent" ? 1 : 100)));
  const reset = typeof source.resetsAt === "number" ? source.resetsAt : Date.parse(source.resets_at) / 1000;
  return { id: type, label: definition[0], usedPercent, remainingPercent: 100 - usedPercent,
    windowDurationMins: definition[1], ...(Number.isFinite(reset) && reset > 0 ? { resetsAt: Math.floor(reset) } : {}), checkedAtMs: timestamp };
}

// How full the context window is after a reply, in Codex's
// `thread/tokenUsage/updated` shape: the last request's tokens (what Claude
// Code's own context meter counts) against the window the result reports for
// that model. Unknown when either is missing.
export function contextUsage(last, modelUsage) {
  const usage = last?.usage;
  if (!usage || !modelUsage || typeof modelUsage !== "object") return null;
  // Results can include fallback and subagent models. Use the native key or
  // an unambiguous canonical-model mapping, never another model's largest window.
  if (typeof last.model !== "string" || !last.model) return null;
  const direct = Object.hasOwn(modelUsage, last.model) ? modelUsage[last.model] : null;
  const matches = direct ? [direct] : Object.values(modelUsage).filter(value => value?.canonicalModel === last.model);
  const windows = new Set(matches.map(value => value?.contextWindow));
  const window = windows.size === 1 ? [...windows][0] : null;
  if (!Number.isFinite(window) || window <= 0) return null;
  const count = value => Number.isFinite(value) && value > 0 ? value : 0;
  const input = count(usage.input_tokens) + count(usage.cache_read_input_tokens) + count(usage.cache_creation_input_tokens);
  const output = count(usage.output_tokens);
  if (!window || input + output <= 0) return null;
  const breakdown = { totalTokens: input + output, inputTokens: input, cachedInputTokens: count(usage.cache_read_input_tokens), outputTokens: output, reasoningOutputTokens: 0 };
  // The model names the window for this model's later replies written
  // elsewhere, whose transcript holds tokens but no window.
  return { last: breakdown, total: breakdown, modelContextWindow: window, ...(typeof last.model === "string" ? { model: last.model } : {}) };
}

// The SDK's summary already computes its own context usage. Do not recreate
// its categories, policy window or local estimates in Wonder.
export function nativeContextUsage(summary) {
  const used = summary?.totalTokens, window = summary?.rawMaxTokens;
  if (!Number.isSafeInteger(used) || used < 0 || !Number.isSafeInteger(window) || window <= 0
    || typeof summary.model !== "string" || !summary.model) return null;
  return { last: { totalTokens: used }, modelContextWindow: window, model: summary.model };
}

// A boundary is positive evidence of completed compaction. Its content and
// summary are private context and never enter a timeline item.
export function compactionItem(message) {
  if (message?.type !== "system" || message.subtype !== "compact_boundary") return null;
  if (typeof message.uuid !== "string" || !message.uuid) return null;
  // Native history items share the host's ordered import clock. Giving only
  // this item a transcript clock would sort it ahead of its surrounding replies.
  return { type: "contextCompaction", id: message.uuid, status: "completed" };
}

export class TurnProjection {
  // A continuation adds to a turn that already ended, such as the reply Claude
  // writes on its own when a background task finishes: its items join that
  // turn under the same IDs history gives them, and it reports no turn end.
  constructor({ threadId, turnId, emit, onSession = () => {}, onUsage = () => {}, onChild = () => {}, promptUuid = null, continuation = false,
    tasks = new ClaudeTaskList() }) {
    Object.assign(this, { threadId, turnId, emit, onSession, onUsage, onChild, promptUuid, continuation, tasks });
    this.items = new Map(); this.textByMessage = new Map(); this.blocks = new Map(); this.agentTools = new Map();
    this.thinking = new Map(); // running thinking block's text so far, by item ID
    this.messageId = null; this.terminal = false; this.completedTexts = new Set();
  }
  notify(method, params = {}, threadId = this.threadId, turnId = this.turnId) {
    this.emit({ method, params: { threadId, turnId, ...params, agentFamily: "claude" } });
  }
  start() { this.notify("turn/started", { turn: { id: this.turnId, status: "inProgress", items: [] } }); }
  startItem(item) {
    if (this.items.has(item.id)) return this.items.get(item.id);
    this.items.set(item.id, item);
    this.notify("item/started", { item: { ...item } });
    return item;
  }
  finishItem(item, status = item.success === false ? "failed" : "completed") {
    if (["completed", "failed", "interrupted"].includes(item.status)) return;
    item.status = status;
    this.notify("item/completed", { item: { ...item } });
  }
  accept(message) {
    // A background task can end while no turn runs, or after this turn ended.
    if (!message.parent_tool_use_id) this.taskEvent(message);
    if (this.terminal) return;
    if (message.parent_tool_use_id) { this.onChild(message); return; }
    if (message.type === "system" && message.subtype === "init") this.onSession(message.session_id);
    if (message.type === "rate_limit_event") {
      const window = usageWindow(message.rate_limit_info?.rateLimitType, message.rate_limit_info);
      if (window) this.onUsage(window);
      return;
    }
    if (message.type === "system" && message.subtype === "compact_boundary") {
      // The old request no longer describes this session's context.
      this.lastUsage = null; this.nativeContext = undefined;
      this.notify("thread/tokenUsage/updated", { tokenUsage: null });
      const item = compactionItem(message);
      if (item && !this.items.has(item.id)) {
        this.items.set(item.id, item);
        this.notify("item/completed", { item: { ...item } });
      }
      return;
    }
    if (message.type === "system" && ["task_started", "task_progress", "task_notification"].includes(message.subtype)) {
      this.onChild(message);
      return;
    }
    if ([message.user_message_uuid, ...(message.user_message_uuids ?? [])].includes(this.promptUuid)) this.promptSeen = true;
    if (message.type === "stream_event") this.stream(message.event);
    if (message.type === "assistant") {
      if (Object.hasOwn(message, "context_usage")) {
        const usage = message.context_usage;
        this.nativeContext = nativeContextUsage({ totalTokens: usage?.total_tokens, rawMaxTokens: usage?.raw_max_tokens, model: usage?.model });
        this.notify("thread/tokenUsage/updated", { tokenUsage: this.nativeContext });
      }
      if (!Object.hasOwn(message, "context_usage") && message.message?.usage) this.nativeContext = undefined;
      const id = message.message?.id;
      if (message.message?.usage) this.lastUsage = { usage: message.message.usage, model: message.message.model };
      for (const block of message.message?.content ?? []) {
        if (block.type === "text") this.completeText(id, block.text);
        if (block.type === "tool_use") this.startTool(block);
      }
    }
    if (message.type === "user" && Array.isArray(message.message?.content)) {
      // Runtime synthetic user instructions are not messages written by the owner.
      for (const block of message.message.content) if (block.type === "tool_result") this.toolResult(block, message.tool_use_result);
    }
    if (message.type === "result") {
      // A resumed session can first finish work queued before this prompt,
      // such as a background-task notice left by a desktop run. Claude starts
      // that turn itself, so its result names no prompt. Only the result that
      // answers this prompt, or one after a reply to it began, ends the turn.
      const answered = [message.user_message_uuid, ...(message.user_message_uuids ?? [])].filter(Boolean);
      const success = message.subtype === "success" && !message.is_error;
      // Session-level startup/crash errors carry no prompt identity. Surface
      // them instead of leaving the user waiting forever for a reply.
      if (this.promptUuid && !answered.includes(this.promptUuid)
        && (answered.length || (success && !this.promptSeen))) return;
      this.structuredOutput = message.structured_output;
      const tokenUsage = contextUsage(this.lastUsage, message.modelUsage);
      // Missing optional data without a new reply is not a reset. A measured
      // reply whose window is unknown must clear the prior meter instead.
      if (this.nativeContext === undefined && this.lastUsage !== undefined) {
        this.notify("thread/tokenUsage/updated", { tokenUsage });
      }
      this.finish(success ? "completed" : "failed", success ? undefined : (message.errors?.[0] ?? message.result ?? "Claude could not complete this response."));
    }
  }
  stream(event) {
    if (!event) return;
    if (event.type === "message_start") {
      for (const item of this.blocks.values()) {
        if (item?.type !== "reasoning" || item.status !== "inProgress") continue;
        item.status = "interrupted";
        this.notify("item/completed", { item: { ...item } });
      }
      this.messageId = event.message?.id; this.blocks.clear(); this.thinking.clear();
    }
    if (event.type === "content_block_start") {
      if (["thinking", "redacted_thinking"].includes(event.content_block?.type) && this.messageId && Number.isInteger(event.index)
        && event.index >= 0 && event.index < 4096 && !this.blocks.has(event.index)) {
        const id = `${this.turnId}:${this.messageId}:thinking:${event.index}`;
        const item = this.startItem({ type: "reasoning", id, status: "inProgress" });
        this.blocks.set(event.index, item);
        // Redacted thinking is encrypted: lifecycle only.
        if (event.content_block.type === "thinking" && item.status === "inProgress") this.thinking.set(id, String(event.content_block.thinking ?? ""));
      }
      if (event.content_block?.type === "text") {
        const id = `${this.turnId}:${this.messageId}:${event.index}`;
        const item = this.startItem({ type: "agentMessage", id, text: event.content_block.text ?? "", status: "inProgress" });
        this.blocks.set(event.index, item);
        const texts = this.textByMessage.get(this.messageId) ?? [];
        texts.push(item); this.textByMessage.set(this.messageId, texts);
      }
    }
    if (event.type === "content_block_stop") {
      const item = this.blocks.get(event.index);
      if (item?.type === "reasoning") {
        // The text travels once, with the completed block; never while it runs.
        const summary = item.status === "inProgress" ? thinkingSummary(this.thinking.get(item.id)) : [];
        this.thinking.delete(item.id);
        if (summary.length) item.summary = summary;
        this.finishItem(item);
      }
      else if (!item && Number.isInteger(event.index) && event.index >= 0 && event.index < 4096) this.blocks.set(event.index, null);
    }
    if (event.type === "content_block_delta" && event.delta?.type === "thinking_delta") {
      const item = this.blocks.get(event.index);
      if (item?.type === "reasoning" && this.thinking.has(item.id) && typeof event.delta.thinking === "string") {
        this.thinking.set(item.id, this.thinking.get(item.id) + event.delta.thinking);
      }
      return;
    }
    if (event.type === "content_block_delta" && event.delta?.type === "text_delta") {
      const item = this.blocks.get(event.index);
      if (item?.type !== "agentMessage" || typeof event.delta.text !== "string") return;
      item.text += event.delta.text;
      this.notify("item/agentMessage/delta", { itemId: item.id, delta: event.delta.text });
    }
  }
  completeText(messageId, text) {
    if (typeof text !== "string") return;
    const key = `${messageId}\0${text}`;
    if (this.completedTexts.has(key)) return;
    this.completedTexts.add(key);
    let item = (this.textByMessage.get(messageId) ?? []).find(i => i.status === "inProgress" && (i.text === text || text.startsWith(i.text)));
    if (!item) item = this.startItem({ type: "agentMessage", id: `${this.turnId}:${messageId}:text:${this.completedTexts.size}`, text: "", status: "inProgress" });
    item.text = text; this.finishItem(item);
  }
  startTool(block) {
    if (block.name === "ExitPlanMode") {
      // The owner reads the plan in the conversation; the tool result is not a row.
      const plan = planItem(block, "inProgress");
      if (plan) this.finishItem(this.startItem(plan));
      return;
    }
    // Text before this tool call (an agent launch too) was progress; restate
    // it as commentary.
    for (const item of markCommentary([...this.items.values(), { type: "tool" }])) {
      if (item.status === "completed") this.notify("item/completed", { item: { ...item } });
    }
    if (["Agent", "Task"].includes(block.name)) {
      // The verified task lifecycle supplies the existing agent activity row.
      // Launch acknowledgements contain internal IDs/paths, not a user reply.
      this.agentTools.set(block.id, block);
      return;
    }
    const wonder = block.name?.startsWith("mcp__wonder__") ? block.name.slice("mcp__wonder__".length) : null;
    this.startItem(wonder
      ? { type: "dynamicToolCall", id: block.id, tool: wonder, arguments: block.input, status: "inProgress" }
      : claudeToolItem(block, "inProgress")
        || { type: "mcpToolCall", id: block.id, server: "Claude", tool: block.name, arguments: block.input, status: "inProgress" });
  }
  toolResult(block, result) {
    if (this.agentTools.has(block.tool_use_id)) {
      this.onChild({ type: "agent_result", tool_use_id: block.tool_use_id, result, is_error: block.is_error, content: block.content });
      return;
    }
    const item = this.items.get(block.tool_use_id);
    if (!item || ["completed", "failed"].includes(item.status)) return;
    const changedTasks = applyClaudeToolResult(item, block, result, this.tasks, { holdBackground: true });
    if (item.backgroundTaskId && item.status === "inProgress") {
      // Launched, not finished: a notice, a read or a kill settles it later.
      trackBackground(this.threadId, this.turnId, item);
      return;
    }
    item.status = "inProgress"; // finishItem publishes the terminal state.
    this.finishItem(item);
    const read = backgroundReadOutcome(item, toolContent(block.content).map(c => c.text ?? "").join("\n"), result);
    if (read) this.settleBackground(read.targetId, read);
    if (changedTasks) {
      const id = `${this.turnId}:tasks`, checklist = taskChecklistItem(this.items.get(id), this.turnId, this.tasks.snapshot());
      if (!this.items.has(id)) this.startItem({ ...checklist, status: "inProgress" });
      this.items.set(id, checklist);
      this.notify("item/completed", { item: { ...checklist } });
    }
  }
  // Settles one launched background item (by tool-use or task ID) and publishes
  // it under the turn that started it. False when nothing is pending for it.
  settleBackground(key, outcome) {
    const entry = pendingFor(typeof key === "string" ? { toolUseId: key, taskId: key } : key);
    if (!entry || !settleBackgroundItem(entry.item, outcome)) return false;
    pendingBackground.delete(entry.item.id);
    this.notify("item/completed", { item: { ...entry.item } }, entry.threadId, entry.turnId);
    return true;
  }
  // Ends every background item still pending for these conversations (this one
  // by default), such as when their process exits and no notice can follow.
  settleAllBackground(status = "interrupted", error, threads = [this.threadId]) {
    let settled = false;
    for (const entry of [...pendingBackground.values()])
      if (threads.includes(entry.threadId)) settled = this.settleBackground(entry.item.id, { status, error }) || settled;
    return settled;
  }
  // The SDK's task_notification and task_updated events, and the queued
  // <task-notification> message, each end a launched background item. True
  // when one did, so a caller outside a running turn knows to persist it.
  taskEvent(message) {
    if (message?.ambient || message?.skip_transcript) return false;
    if (message?.type === "system" && message.subtype === "task_notification") {
      return this.settleBackground({ toolUseId: message.tool_use_id, taskId: message.task_id },
        { status: outcomeStatus(message.status), error: message.summary });
    }
    if (message?.type === "system" && message.subtype === "task_updated") {
      const status = outcomeStatus(message.patch?.status);
      return Boolean(status) && this.settleBackground({ taskId: message.task_id }, { status, error: message.patch.error });
    }
    if (message?.type === "user" && !message.parent_tool_use_id) {
      const content = message.message?.content;
      const text = typeof content === "string" ? content
        : Array.isArray(content) ? content.filter(b => b?.type === "text").map(b => b.text ?? "").join("\n") : "";
      const notice = parseTaskNotice(text);
      return Boolean(notice) && this.settleBackground(notice, { status: notice.status, error: notice.summary });
    }
    return false;
  }
  finish(status, error) {
    if (this.terminal) return;
    this.terminal = true;
    this.thinking.clear();
    for (const item of this.items.values()) {
      if (item.type === "reasoning" && item.status === "inProgress") {
        // A turn ending without the matching block stop is not timed thinking.
        item.status = "interrupted";
        this.notify("item/completed", { item: { ...item } });
      }
      else if (item.type === "agentMessage") this.finishItem(item);
      // A launched background task outlives the turn that started it.
      else if (item.status === "inProgress" && !isPendingBackground(item)) {
        item.success = false;
        item.error = { message: status === "interrupted" ? "Action was cancelled." : "Action did not return a confirmed result." };
        this.finishItem(item, status === "interrupted" ? "interrupted" : "failed");
      }
    }
    if (this.continuation) return;
    this.result = { id: this.turnId, status, ...(this.structuredOutput === undefined ? {} : { structuredOutput: this.structuredOutput }), items: [...this.items.values()], error: error ? { message: String(error).slice(0, 2000) } : null };
    this.notify("turn/completed", { turn: this.result });
  }
}

import { drainSteps } from "./cooperative.mjs";
// Native Claude Code transcripts are authoritative for project conversations.
// Project SDK session messages into the same turn/item shapes as the live
// stream so Wonder reconciles both by stable IDs instead of duplicating them.
import { execFile } from "node:child_process";
import { lstat, open, opendir } from "node:fs/promises";
import { constants } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import { applyClaudeToolResult, backgroundReadOutcome, ClaudeTaskList, claudeToolItem, compactionItem, markCommentary, parseTaskNotice, planItem,
  settleBackgroundItem, taskChecklistItem, thinkingSummary, toolContent } from "./projection.mjs";

const textOf = content => typeof content === "string" ? content
  : Array.isArray(content) ? content.filter(b => b?.type === "text").map(b => b.text ?? "").join("\n") : "";
const tag = (text, name) => text.match(new RegExp(`<${name}>([\\s\\S]*?)</${name}>`))?.[1]?.trim() || null;
const bounded = (value, limit) => typeof value === "string" && value.trim() ? value.trim().slice(0, limit) : null;

// Claude Code delivers background-task completions as queued "user" messages.
// They are runtime events, not words written by the owner.
export function taskNotification(entry) {
  if (entry?.type !== "user") return null;
  const text = textOf(entry.message?.content);
  if (entry.origin?.kind !== "task-notification" && !text.trimStart().startsWith("<task-notification>")) return null;
  const body = tag(text, "task-notification") ?? text;
  return { taskId: tag(body, "task-id"), toolUseId: tag(body, "tool-use-id"), status: tag(body, "status"),
    summary: tag(body, "summary"), outputFile: tag(body, "output-file") };
}
const unescapeXml = text => text.replace(/&lt;/g, "<").replace(/&gt;/g, ">").replace(/&quot;/g, "\"")
  .replace(/&apos;|&#39;/g, "'").replace(/&amp;/g, "&");
const WAKE_VERBS = { completed: "finished", failed: "failed", interrupted: "stopped" };
// The row a background task's report leaves in the conversation. Claude Code
// fills <summary> with `Background command "<description>" completed (exit code 0)`,
// `Agent "<description>" completed` or `Monitor "<description>" ended`; the row
// says `Command "<description>" finished` (or failed/stopped) in plain words.
export function wakeItem(entry, notice) {
  const status = parseTaskNotice(textOf(entry.message?.content))?.status ?? null;
  const summary = notice.summary ? unescapeXml(notice.summary) : null;
  const named = summary && /^(Background command|Command|Agent|Monitor)\s+"([\s\S]*)"(?:\s+[^"]*)?$/.exec(summary);
  const kind = named ? { agent: "agent", monitor: "monitor" }[named[1].toLowerCase()] ?? "command" : "command";
  const verb = status === "completed" && kind === "monitor" ? "ended" : WAKE_VERBS[status];
  const text = named && verb ? `${kind[0].toUpperCase()}${kind.slice(1)} "${named[2].trim().slice(0, 200)}" ${verb}`
    : summary ? summary.slice(0, 200) : `Background task ${verb ?? "update"}`;
  return { type: "taskNotification", id: entry.uuid, status: "completed", taskStatus: status ?? "completed", kind, text };
}
// SDK readers use is_meta; older native records use isMeta. These flags and
// isCompactSummary identify harness text without guessing from its wording.
export const metaMessage = entry => entry?.is_meta === true || entry?.isMeta === true || entry?.isCompactSummary === true;
const syntheticUser = entry => metaMessage(entry) || Boolean(entry?.origin?.kind && entry.origin.kind !== "human") || taskNotification(entry) !== null;
// Claude Code's desktop app appends each paste to the prompt wrapped as
// `<pasted_content id="x">...</pasted_content id="x">` (the closing tag repeats
// the id). The tags are transport markup; the owner sees the pasted words.
export const unwrapPastes = text => !text.includes("<pasted_content") ? text : text.replace(
  /<pasted_content id="([^"]*)">\n?([\s\S]*?)\n?<\/pasted_content id="\1">/g, (_, _id, body) => body).trim();
// Claude Code and the Claude desktop app add their own context to a user turn as
// whole tagged blocks on their own lines (worktree and linked-session reminders,
// messages from other sessions, CI and side-session events). The owner did not
// write them. Only whole blocks that start and end a line, outside a paste, are
// removed, so a tag the owner mentions inline or pastes stays.
const pasteBlock = /(<pasted_content id="([^"]*)">[\s\S]*?<\/pasted_content id="\2">)/;
const harnessBlock = new RegExp(String.raw`(^|\n)[ \t]*<(system-reminder|cross-session-message|side-sessions-event|ci-monitor-event|session-mentions|local-command-caveat)(?:\s[^>\n]*)?>[\s\S]*?</\2>[ \t]*(?=\n|$)`, "g");
export const stripHarness = text => {
  if (!text.includes("<")) return text;
  // split returns text, paste, paste id, text, ...; only the text between pastes is stripped.
  const parts = text.split(pasteBlock);
  const stripped = parts.map((part, i) => i % 3 === 0 ? part.replace(harnessBlock, "$1") : i % 3 === 1 ? part : "").join("");
  return stripped === text ? text : stripped.trim();
};
// What the owner wrote in a user entry: harness blocks removed, pastes unwrapped.
export const ownerText = content => unwrapPastes(stripHarness(textOf(content)));
// Images the owner attached in Claude Code or the Claude desktop app. They
// travel as the transcript's own base64 bytes; wonderd verifies and stores
// them as the bubble's attachments. Bounded per turn and per image (8 MB).
const MAX_PROMPT_IMAGES = 8, MAX_IMAGE_BASE64 = Math.ceil(8 * 1024 * 1024 / 3) * 4;
export const ownerImages = content => !Array.isArray(content) ? [] : content
  .filter(b => b?.type === "image" && b.source?.type === "base64" && typeof b.source.data === "string"
    && typeof b.source.media_type === "string")
  .slice(0, MAX_PROMPT_IMAGES)
  .map(b => b.source.data.length <= MAX_IMAGE_BASE64
    ? { type: "image", mimeType: b.source.media_type, data: b.source.data }
    : { type: "image", mimeType: b.source.media_type, unavailable: "This image is larger than 8 MB" });
const interruptMarker = text => /^\[Request interrupted by user/.test(text.trim());
// A `!` shell command or slash command run in Claude Code is one user entry
// (`<bash-input>` or `<command-name>`) and its output a second one
// (`<bash-stdout>`, `<local-command-stdout>`, ...). The output belongs to the
// command's turn; the phone renders the joined markup as one command card.
// T3 Code compacts a thread by sending `/compact` as a message. It is the
// request for the compaction divider, not words to show: the divider comes
// from the boundary entry, and the command's arguments and output stay private.
const compactCommand = text => /^\s*(\/compact(\s|$)|<command-name>\s*\/compact\s*<\/command-name>)/.test(text);
const commandOutput = text => /^<(bash-stdout|bash-stderr|local-command-stdout|local-command-stderr)>/.test(text.trimStart());

// The prompt that opens a turn: a human message, not a tool result, background
// notice or interruption marker. `nativeTurns` splits history at the same entries.
const opensTurn = entry => {
  if (entry?.type !== "user" || entry.parent_tool_use_id) return false;
  const content = entry.message?.content;
  if (Array.isArray(content) && content.some(b => b?.type === "tool_result")) return false;
  const text = ownerText(content);
  if (!text.trim()) return ownerImages(content).length > 0 && !syntheticUser(entry);
  return !syntheticUser(entry) && !interruptMarker(text) && !commandOutput(text) && !compactCommand(text);
};

// The task reports that start a run of their own: ones that arrive once the
// previous run has ended. Claude Code writes a report that lands mid-run (a
// queued command after a tool result) into the transcript too; that one is
// part of the running turn and opens nothing. A run ends at an assistant
// message that stops without a tool call, or at an interruption marker.
const REPLY_STOPS = ["end_turn", "stop_sequence", "max_tokens", "refusal"];
export function wakeStarts(messages) {
  const starts = new Set(); let idle = true;
  for (const entry of messages) {
    if (entry?.parent_tool_use_id) continue;
    const content = entry?.message?.content;
    if (entry?.type === "assistant") {
      const blocks = Array.isArray(content) ? content : [];
      const stop = entry.message?.stop_reason ?? null;
      if (blocks.some(b => b?.type === "tool_use")) idle = false;
      else if (REPLY_STOPS.includes(stop) || (stop == null && blocks.some(b => b?.type === "text" && b.text))) idle = true;
    } else if (entry?.type === "user") {
      if (Array.isArray(content) && content.some(b => b?.type === "tool_result")) idle = false;
      else if (taskNotification(entry)) { if (idle && entry.uuid) starts.add(entry); }
      else if (interruptMarker(ownerText(content))) idle = true;
      else if (opensTurn(entry)) idle = false;
    }
  }
  return starts;
}

// The last transcript entry that belongs to the turn `turnId` (the uuid of the
// prompt that opened it), so a fork can end exactly where that turn ends.
export function turnEndMessageId(messages, turnId) {
  let found = false, last = null;
  const wakes = wakeStarts(messages);
  for (const entry of messages) {
    if (opensTurn(entry) || wakes.has(entry)) { if (found) break; found = entry.uuid === turnId; }
    if (found && !entry.parent_tool_use_id && typeof entry.uuid === "string") last = entry.uuid;
  }
  return last;
}

// Where Edit from here cuts the transcript for the turn `turnId`: the index
// of the prompt that opened it, and the last main-chain entry before it (the
// point a resumed session continues from), null when it is the first turn.
export function rewindPoint(messages, turnId) {
  const wakes = wakeStarts(messages);
  const index = messages.findIndex(entry => (opensTurn(entry) || wakes.has(entry)) && entry.uuid === turnId);
  if (index < 0) return null;
  let keep = null;
  for (let i = index - 1; i >= 0 && keep == null; i--)
    if (!messages[i]?.parent_tool_use_id && typeof messages[i]?.uuid === "string") keep = messages[i].uuid;
  return { index, keep };
}

// Wonder-originated turns reuse the owner's receipt from the bridge journal.
// A background command or monitor stays in progress until a later transcript
// entry settles it (its task notice, a read of it or a kill). One left over
// takes its outcome from `liveTasks` (the session's task list: { id, status }),
// stays in progress while that says it runs, and otherwise ends interrupted.
// Without a task list it runs only while the session is `active`.
export function nativeTurns(messages, journal = [], active = false, liveTasks = null) {
  const steps = nativeTurnsSteps(messages, journal, active, liveTasks); let next;
  while (!(next = steps.next()).done) { /* Synchronous model/fixture callers. */ }
  return next.value;
}
export const nativeTurnsAsync = (messages, journal = [], active = false, liveTasks = null) => drainSteps(nativeTurnsSteps(messages, journal, active, liveTasks));

function* nativeTurnsSteps(messages, journal = [], active = false, liveTasks = null) {
  const turns = [], known = new Map(journal.map(turn => [turn.id, turn]));
  const blockIndex = new Map(), tools = new Map(), toolTurns = new Map(), tasks = new ClaudeTaskList();
  const held = new Map(); // launched background items not yet settled, by tool-use ID
  const heldFor = ({ toolUseId, taskId }) => held.get(toolUseId) ?? (taskId ? [...held.values()].find(i => i.backgroundTaskId === taskId) : undefined);
  const settleHeld = (key, outcome) => {
    const item = heldFor(key);
    if (item && settleBackgroundItem(item, outcome)) held.delete(item.id);
  };
  let turn = null, compactRequested = false;
  const wakes = wakeStarts(messages);
  for (const entry of messages) {
    yield;
    const content = entry?.message?.content;
    if (entry?.parent_tool_use_id) continue; // Child-agent detail stays in its own activity.
    const boundary = compactionItem(entry);
    if (boundary) {
      if (!turn) { turn = { id: boundary.id, status: "completed", error: null, items: [] }; turns.push(turn); }
      if (!turn.items.some(item => item.id === boundary.id)) turn.items.push(boundary);
      continue;
    }
    if (entry?.type === "user") {
      const results = Array.isArray(content) ? content.filter(b => b?.type === "tool_result") : [];
      for (const block of results) {
        const item = tools.get(block.tool_use_id);
        if (!item) continue;
        const changedTasks = applyClaudeToolResult(item, block, entry.tool_use_result, tasks, { holdBackground: true });
        if (item.type === "mcpToolCall") delete item.result;
        if (item.backgroundTaskId && item.status === "inProgress") held.set(item.id, item);
        const read = backgroundReadOutcome(item, toolContent(block.content).map(c => c.text ?? "").join("\n"), entry.tool_use_result);
        if (read) settleHeld({ taskId: read.targetId }, read);
        const owner = toolTurns.get(block.tool_use_id);
        if (changedTasks && owner) {
          // One checklist per turn, where its first change happened.
          const at = owner.items.findIndex(i => i.id === `${owner.id}:tasks`);
          const updatedAt = Date.parse(entry.timestamp ?? "") || 0;
          const checklist = taskChecklistItem(at >= 0 ? owner.items[at] : null, owner.id, tasks.snapshot(), updatedAt);
          if (at >= 0) owner.items[at] = checklist; else owner.items.splice(owner.items.indexOf(item) + 1, 0, checklist);
        }
      }
      const report = results.length ? null : taskNotification(entry);
      const notice = results.length ? null : parseTaskNotice(textOf(content));
      if (notice) settleHeld(notice, { status: notice.status, error: notice.summary });
      if (report && wakes.has(entry)) {
        // A wake starts the agent's next run: its row, then the run's own work
        // and reply, apart from the owner's turn it follows.
        turn = { id: entry.uuid, status: "completed", error: null, items: [wakeItem(entry, report)] };
        turns.push(turn);
        continue;
      }
      if (notice) continue;
      const text = ownerText(content);
      const images = results.length ? [] : ownerImages(content);
      if (results.length || (!text.trim() && !images.length) || syntheticUser(entry)) continue;
      if (interruptMarker(text)) { if (turn) turn.interrupted = true; continue; }
      if (compactCommand(text)) { compactRequested = true; continue; }
      if (compactRequested && commandOutput(text)) continue;
      compactRequested = false;
      const prompt = turn?.items[0];
      if (commandOutput(text) && prompt?.type === "userMessage" && prompt.content?.[0]?.type === "text") {
        prompt.content = [{ ...prompt.content[0], text: `${prompt.content[0].text}\n${text.trim()}` }];
        continue;
      }
      const receipt = known.get(entry.uuid);
      const user = receipt?.items?.find(i => i.type === "userMessage");
      turn = { id: entry.uuid, status: "completed", error: null, items: [user
        ? { ...user }
        : { type: "userMessage", id: entry.uuid, content: [...(text.trim() ? [{ type: "text", text }] : []), ...images] }] };
      turns.push(turn);
      continue;
    }
    if (entry?.type !== "assistant" || !turn || !Array.isArray(content)) continue;
    const messageId = entry.message?.id ?? entry.uuid;
    for (const block of content) {
      const index = blockIndex.get(messageId) ?? 0;
      blockIndex.set(messageId, index + 1);
      if (block?.type === "text" && block.text) {
        turn.items.push({ type: "agentMessage", id: `${turn.id}:${messageId}:${index}`, text: block.text, status: "completed" });
      } else if (block?.type === "thinking") {
        // Same item as the live stream's finished thinking block. Redacted or
        // empty thinking has no text to show and adds no row.
        const summary = thinkingSummary(block.thinking);
        if (summary.length) turn.items.push({ type: "reasoning", id: `${turn.id}:${messageId}:thinking:${index}`, status: "completed", summary });
      } else if (block?.type === "tool_use" && block.name === "ExitPlanMode") {
        // Same item ID as the live stream, so history and events reconcile.
        const plan = planItem(block, "completed");
        if (plan) turn.items.push(plan);
      } else if (block?.type === "tool_use" && !["Agent", "Task"].includes(block.name)) {
        const item = claudeToolItem(block, "unknown")
          ?? { type: "mcpToolCall", id: block.id, server: "Claude", tool: block.name, arguments: block.input ?? {}, status: "unknown" };
        tools.set(block.id, item); toolTurns.set(block.id, turn);
        turn.items.push(item);
      }
    }
  }
  for (const item of held.values()) {
    const task = liveTasks?.find(t => t.id === item.id);
    if (liveTasks ? task?.status === "running" : active) continue;
    settleBackgroundItem(item, { status: ["completed", "failed"].includes(task?.status) ? task.status : "interrupted", error: task?.summary });
  }
  for (const t of turns) markCommentary(t.items);
  const last = turns.at(-1);
  for (const t of turns) if (t.interrupted) { t.status = "interrupted"; delete t.interrupted; }
  if (last && active) {
    last.status = "inProgress";
    // A call still waiting for its result is running, not done.
    for (const item of last.items) if (tools.get(item.id) === item && item.success === undefined) item.status = "inProgress";
  }
  // Preserve the durable outcome of Wonder's own turns (failed, interrupted).
  for (const t of turns) {
    const receipt = known.get(t.id);
    // A wake's saved status is only the placeholder it was journaled with; the
    // transcript says whether the run ended or was stopped.
    if (receipt && receipt.status !== "inProgress" && !(t === last && active) && t.items[0]?.type !== "taskNotification") { t.status = receipt.status; t.error = receipt.error ?? null; }
    // Optional SDK hints are not transcript messages. Keep their observed
    // source turn in the existing journal, never append them as chat bubbles.
    if (t === last && t.status === "completed" && !active) {
      const hint = receipt?.items?.find(i => i.type === "promptSuggestion");
      if (hint) t.items.push(hint);
    }
  }
  return turns;
}

export function sessionSummary(info, isComputerSession) {
  const title = (info.customTitle || info.summary || stripHarness(info.firstPrompt ?? "") || "Claude session").replace(/\s+/g, " ").trim().slice(0, 200);
  return { sessionId: info.sessionId, title, cwd: info.cwd ?? null,
    ...(typeof isComputerSession === "boolean" ? { isComputerSession } : {}),
    createdAt: Math.floor((info.createdAt ?? info.lastModified) / 1000), updatedAt: Math.floor(info.lastModified / 1000) };
}

const taskStatus = status => ({ completed: "completed", failed: "failed", error: "failed", killed: "interrupted",
  stopped: "interrupted", cancelled: "interrupted" })[String(status ?? "").toLowerCase()] ?? "running";

// Every Bash run and agent task in the session, plus background commands,
// monitors and linked sessions, newest first, for the agent-task list: the
// rows Claude's own Background tasks panel shows. A task still marked running
// in a session that is no longer working on the Mac lost its completion
// notice; its state is unknown, not running.
export const MAX_SESSION_TASKS = 1000;
const interruptedTool = text => /^\[Request interrupted by user|^The user doesn't want to proceed/i.test(text ?? "");
const seconds = (start, end) => {
  const span = Date.parse(end ?? "") - Date.parse(start ?? "");
  return Number.isFinite(span) && span >= 0 ? Math.round(span / 1000) : null;
};
// `since` is when the Claude process holding the session now started (ms):
// a task launched before it belonged to a process that has exited, which
// ended it without a notice, so it is not running.
export function backgroundTasks(messages, live = false, agentFiles = [], options = {}) {
  const steps = backgroundTasksSteps(messages, live, agentFiles, options); let next;
  while (!(next = steps.next()).done) { /* Synchronous model/fixture callers. */ }
  return next.value;
}
export const backgroundTasksAsync = (messages, live = false, agentFiles = [], options = {}) => drainSteps(backgroundTasksSteps(messages, live, agentFiles, options));

function* backgroundTasksSteps(messages, live = false, agentFiles = [], { since = null } = {}) {
  const tasks = new Map();
  // TaskStop/KillShell and TaskOutput/BashOutput calls, by tool-use ID: a stop
  // ends its task without any later notice, and a read may report the end.
  const controls = new Map();
  // Claude Code's task files outlive compaction; transcript entries refine them.
  for (const file of [...agentFiles].sort((a, b) => String(a.startedAt ?? "").localeCompare(String(b.startedAt ?? "")))) {
    tasks.set(file.toolUseId, { id: file.toolUseId, kind: "agent", background: file.background,
      title: bounded(file.description, 200) ?? "Agent task", role: bounded(file.agentType, 80),
      status: file.finished ? "completed" : "running", request: null, summary: null, result: null,
      outputFile: null, taskId: file.agentId, startedAt: file.startedAt ?? null, currentActivity: file.currentActivity ?? null });
  }
  for (const entry of messages) {
    yield;
    if (entry?.parent_tool_use_id || entry?.isSidechain) continue;
    const content = entry?.message?.content;
    if (entry?.type === "assistant" && Array.isArray(content)) {
      for (const block of content) {
        if (block?.type !== "tool_use" || typeof block.id !== "string") continue;
        if (["TaskStop", "KillShell", "TaskOutput", "BashOutput"].includes(block.name)) {
          controls.set(block.id, { server: "Claude", tool: block.name, arguments: block.input ?? {} });
          continue;
        }
        const agent = ["Agent", "Task"].includes(block.name);
        const known = tasks.get(block.id);
        // A Monitor is always a background watch and counts among the
        // session's tasks, as in Claude. A linked session is its own chat, not
        // a task (Claude's Background tasks leaves spawned sessions out).
        if (block.name === "Monitor") {
          tasks.set(block.id, { id: block.id, kind: "monitor", background: true,
            title: bounded(block.input?.description, 200) ?? "Monitor",
            role: null, status: "running", request: bounded(block.input?.command, 8000),
            summary: null, result: null, outputFile: null, taskId: null, launched: false,
            startedAt: entry.timestamp ?? null });
          continue;
        }
        const background = block.input?.run_in_background === true || known?.background === true;
        // A foreground Bash run is part of the reply and is not a task; it is
        // followed only in case Claude moves it to the background.
        if (!agent && !background && block.name !== "Bash") continue;
        const kind = agent ? "agent" : background ? "command" : "bash";
        tasks.set(block.id, { ...known, id: block.id, kind, background,
          title: bounded(block.input?.description, 200) ?? known?.title
            ?? (agent ? "Agent task" : bounded(block.input?.command?.split?.("\n")[0], 80) ?? (background ? "Background command" : "Bash")),
          role: agent ? bounded(block.input?.subagent_type, 80) ?? known?.role ?? null : null, status: known?.status ?? "running",
          request: bounded(agent ? block.input?.prompt : block.input?.command, 8000),
          summary: null, result: null, outputFile: null, taskId: known?.taskId ?? null, startedAt: entry.timestamp ?? known?.startedAt ?? null });
      }
    }
    if (entry?.type !== "user") continue;
    for (const block of Array.isArray(content) ? content : []) {
      const control = block?.type === "tool_result" ? controls.get(block.tool_use_id) : null;
      if (control) {
        const text = toolContent(block.content).filter(c => c.type === "inputText").map(c => c.text).join("\n");
        const outcome = backgroundReadOutcome({ ...control, success: block.is_error !== true }, text, entry.tool_use_result ?? entry.toolUseResult);
        const target = outcome && [...tasks.values()].find(t => t.taskId === outcome.targetId && t.status === "running");
        if (target) Object.assign(target, { status: outcome.status, updatedAt: entry.timestamp ?? target.updatedAt ?? null });
        continue;
      }
      const task = block?.type === "tool_result" ? tasks.get(block.tool_use_id) : null;
      if (!task) continue;
      const text = bounded(toolContent(block.content).filter(c => c.type === "inputText").map(c => c.text).join("\n"), 32_000);
      // Claude Code may run an agent in the background without the explicit flag;
      // its immediate result only acknowledges the launch.
      if (task.kind === "agent" && /^Async agent launched/i.test(text ?? "")) {
        task.background = true;
        task.taskId ??= text.match(/\bagentId: ([A-Za-z0-9_-]{1,64})/)?.[1] ?? null;
        continue;
      }
      if (task.kind === "monitor") {
        // Only a started watch runs; a refusal does not.
        const started = block.is_error !== true && text?.match(/^Monitor started \(task ([A-Za-z0-9_-]{1,64})/);
        if (started) Object.assign(task, { taskId: started[1], launched: true });
        else if (task.status === "running") { task.status = block.is_error === true ? "failed" : "dropped"; task.result = text; }
        continue;
      }
      // A foreground command Claude moved to the background (it ran long, or the
      // user sent it there) continues as a background command.
      const moved = task.kind === "bash" && block.is_error !== true
        ? text?.match(/\bbackground with ID: ([A-Za-z0-9_-]{1,64})|\bmoved to the background \(ID: ([A-Za-z0-9_-]{1,64})\)/) : null;
      if (moved) Object.assign(task, { kind: "command", background: true, taskId: moved[1] ?? moved[2] });
      // A background command's launch names the task ID that stop_task needs.
      if (task.kind === "command" && task.background && block.is_error !== true)
        task.taskId ??= text?.match(/\bbackground with ID: ([A-Za-z0-9_-]{1,64})/)?.[1] ?? null;
      // A background launch only acknowledges the start; the notice carries its outcome.
      if (block.is_error === true) Object.assign(task, { status: interruptedTool(text) ? "interrupted" : "failed", result: text, updatedAt: entry.timestamp ?? null });
      else if (!task.background) Object.assign(task, { status: "completed", result: text, updatedAt: entry.timestamp ?? null });
    }
    const notice = taskNotification(entry);
    if (!notice?.toolUseId) continue;
    // A resumed agent (SendMessage) notifies again under the resuming call's
    // tool-use ID with the same task ID; it is the same task, not a new one.
    let task = tasks.get(notice.toolUseId)
      ?? (notice.taskId ? [...tasks.values()].find(t => t.taskId === notice.taskId) : undefined);
    if (!task) {
      // The launch scrolled out of the readable transcript; the notice still names the task.
      const agent = /^Agent "/.test(notice.summary ?? "");
      task = { id: notice.toolUseId, kind: agent ? "agent" : "command", background: true,
        title: bounded(notice.summary?.match(/"([^"]{1,200})"/)?.[1], 200) ?? "Background task", role: null,
        status: "running", request: null, summary: null, result: null, outputFile: null, taskId: null, startedAt: entry.timestamp ?? null };
      tasks.set(notice.toolUseId, task);
    }
    Object.assign(task, { status: taskStatus(notice.status), summary: bounded(notice.summary, 2000),
      outputFile: notice.outputFile, taskId: notice.taskId ?? task.taskId, updatedAt: entry.timestamp ?? task.updatedAt ?? null });
  }
  // Claude's Background tasks lists what Claude Code gave a task ID: agents,
  // background commands and monitors. A Bash run that stayed in the
  // foreground is part of the reply, not a task.
  const list = [...tasks.values()]
    .filter(task => task.status !== "dropped" && task.kind !== "bash")
    .map(({ launched, ...task }) => ({ ...task, updatedAt: task.updatedAt ?? task.startedAt ?? null,
      durationSeconds: ["completed", "failed", "interrupted"].includes(task.status) ? seconds(task.startedAt, task.updatedAt) : null }))
    .map((task, order) => ({ task, order }))
    .sort((a, b) => String(b.task.startedAt ?? "").localeCompare(String(a.task.startedAt ?? "")) || b.order - a.order)
    .map(({ task }) => task).slice(0, MAX_SESSION_TASKS);
  // A linked session's state is its own process's, not this one's.
  if (!live) for (const task of list) if (task.status === "running") task.status = "unknown";
  if (live && Number.isFinite(since)) for (const task of list) {
    if (task.status === "running" && Date.parse(task.startedAt ?? "") < since) task.status = "interrupted";
  }
  return list;
}

// Whether the session's own reply (its main turn) is still being written, as
// opposed to background work it left running. A turn ends with an assistant
// message that stopped for a reason other than calling a tool, or with an
// interruption; a later prompt, tool result or task notice starts work again.
export function mainTurnRunning(messages) {
  for (let i = messages.length - 1; i >= 0; i--) {
    const entry = messages[i];
    if (entry?.parent_tool_use_id || entry?.isSidechain || metaMessage(entry)) continue;
    if (entry?.type === "assistant") {
      const reason = entry.message?.stop_reason;
      return !reason || reason === "tool_use" || reason === "pause_turn";
    }
    if (entry?.type === "user") return !/^\[Request interrupted by user/.test(textOf(entry.message?.content).trimStart());
  }
  return false;
}

// Opens a native file for reading without following a final symlink.
// Windows has no O_NOFOLLOW: refuse a link found by lstat, then require the
// opened file to be the one lstat saw, so a link swapped in between is refused too.
export async function openNoFollow(path) {
  const flags = constants.O_RDONLY | (constants.O_NOFOLLOW ?? 0) | (constants.O_NONBLOCK ?? 0);
  if (process.platform !== "win32") return open(path, flags);
  const link = await lstat(path, { bigint: true });
  if (link.isSymbolicLink()) throw Object.assign(new Error(`${path} is a symbolic link`), { code: "ELOOP" });
  const handle = await open(path, flags);
  const opened = await handle.stat({ bigint: true }).catch(() => null);
  if (opened?.ino !== link.ino || opened?.dev !== link.dev) {
    await handle.close();
    throw Object.assign(new Error(`${path} changed while opening`), { code: "ELOOP" });
  }
  return handle;
}

// The SDK has no live-state API for another app's process. Claude Code's own
// PID records fill that gap, but partial reads cannot prove a session idle.
// A record that fails validation leaves only the session it names unknown
// (`{ sessionId, uncertain: true }`), so one bad file cannot hold every other
// chat; one that names no session cannot hold any and is skipped. Only limits
// on the whole scan (entries, records, deadline, the process table) fail it.
// Windows reads the process table through PowerShell, measured at about 1.1 s cold.
const SESSION_RECORD_BYTES = 64 * 1024, SESSION_RECORDS = 256, SESSION_READ_MS = process.platform === "win32" ? 3000 : 500;
const sessionReads = new Map();
const validSession = id => typeof id === "string" && id.length > 0 && id.length <= 128;
// The session a damaged or oversized record names, from its readable prefix.
const namedSession = text => /"sessionId"\s*:\s*"([^"\\]{1,128})"/.exec(text)?.[1] ?? null;

async function readSessionRecords(dir) {
  const deadline = Date.now() + SESSION_READ_MS;
  let directory;
  try { directory = await opendir(dir); }
  catch (error) { return error.code === "ENOENT" ? [] : null; }
  const records = [], uncertain = new Set();
  const doubt = id => { if (validSession(id)) uncertain.add(id); };
  let scanned = 0;
  for await (const entry of directory) {
    if (++scanned > SESSION_RECORDS * 4 || Date.now() >= deadline) return null;
    const name = entry.name;
    if (!/^[0-9]{1,10}\.json$/.test(name)) continue;
    const pid = Number(name.slice(0, -5));
    if (!Number.isSafeInteger(pid) || pid <= 0) continue;
    try { process.kill(pid, 0); } catch (error) { if (error.code !== "EPERM") continue; }
    let handle, text = null, record;
    try {
      handle = await openNoFollow(join(dir, name));
      if (!(await handle.stat()).isFile()) continue;
      const buffer = Buffer.alloc(SESSION_RECORD_BYTES + 1);
      const { bytesRead } = await handle.read(buffer, 0, buffer.length, 0);
      text = buffer.subarray(0, Math.min(bytesRead, SESSION_RECORD_BYTES)).toString("utf8");
      if (bytesRead > SESSION_RECORD_BYTES) { doubt(namedSession(text)); continue; }
      record = JSON.parse(text);
    } catch { if (text != null) doubt(namedSession(text)); continue; }
    finally { await handle?.close(); }
    if (!validSession(record?.sessionId)) continue;
    if (record.pid !== pid) { doubt(record.sessionId); continue; }
    records.push(record);
    if (records.length > SESSION_RECORDS) return null;
  }
  const doubts = [...uncertain].map(sessionId => ({ sessionId, uncertain: true }));
  if (!records.length) return doubts;
  const remaining = deadline - Date.now();
  if (remaining <= 0) return null;
  // One bounded native process-table read validates birth time and parentage;
  // caching only by PID would trust a recycled process identifier.
  const processes = await processTable(records.map(r => r.pid), remaining);
  if (processes == null) {
    // ps exits nonzero when every checked process exited during the read.
    return records.every(record => {
      try { process.kill(record.pid, 0); return false; } catch (error) { return error.code !== "EPERM"; }
    }) ? doubts : null;
  }
  const live = new Map(processes.trim().split("\n").map(line => {
    const match = /^\s*(\d+)\s+(\d+)\s+(.+?)\s*$/.exec(line);
    return match ? [Number(match[1]), { parent: Number(match[2]), start: match[3] }] : [null, null];
  }));
  const result = doubts;
  for (const record of records) {
    const nativeProcess = live.get(record.pid);
    if (!nativeProcess) continue;
    if (record.entrypoint === "sdk-ts" && nativeProcess.parent === process.pid) continue;
    if (typeof record.procStart !== "string" || record.procStart.trim() !== nativeProcess.start) {
      if (!uncertain.has(record.sessionId)) { uncertain.add(record.sessionId); result.push({ sessionId: record.sessionId, uncertain: true }); }
      continue;
    }
    result.push({ sessionId: record.sessionId,
      ...(["cli", "claude-desktop"].includes(record.entrypoint)
        && ["idle", "busy", "shell", "waiting"].includes(record.status) ? { isDesktopOwned: true } : {}),
      hostSessionId: typeof record.hostSessionId === "string" ? record.hostSessionId : null,
      status: ["idle", "busy", "shell", "waiting"].includes(record.status) ? record.status : null,
      statusAt: Number.isSafeInteger(record.statusUpdatedAt) && record.statusUpdatedAt >= 0 ? record.statusUpdatedAt : null,
      startedAt: Number.isSafeInteger(record.startedAt) && record.startedAt >= 0 ? record.startedAt : null });
  }
  return result;
}

// "pid ppid start" lines, where start is what Claude Code records as
// `procStart`: `ps -o lstart=` on macOS/Linux, the process creation time as
// a FILETIME (100 ns ticks since 1601, UTC) on Windows. Null when unreadable.
function processTable(pids, timeout) {
  const [file, args] = process.platform === "win32"
    ? ["powershell.exe", ["-NoProfile", "-NonInteractive", "-Command",
      `foreach ($p in Get-CimInstance Win32_Process -Filter '${pids.map(pid => `ProcessId=${Number(pid)}`).join(" OR ")}') {`
      + " try { '{0} {1} {2}' -f $p.ProcessId, $p.ParentProcessId,"
      + " [Diagnostics.Process]::GetProcessById($p.ProcessId).StartTime.ToFileTimeUtc() } catch {} }"]]
    : ["/bin/ps", ["-o", "pid=,ppid=,lstart=", "-p", pids.join(",")]];
  return new Promise(resolve => execFile(file, args,
    { timeout, maxBuffer: 64 * 1024, windowsHide: true, env: { ...process.env, LC_ALL: "C", TZ: "UTC" } },
    (error, out) => resolve(error ? null : String(out).replace(/\r/g, ""))));
}

export async function claudeSessionRecords(dir = join(process.env.CLAUDE_CONFIG_DIR ?? join(homedir(), ".claude"), "sessions")) {
  let read = sessionReads.get(dir);
  if (!read) {
    if (sessionReads.size >= 4) return null;
    read = readSessionRecords(dir).catch(() => null).finally(() => sessionReads.delete(dir));
    sessionReads.set(dir, read);
  }
  let timer;
  try { return await Promise.race([read, new Promise(resolve => { timer = setTimeout(resolve, SESSION_READ_MS, null); })]); }
  finally { clearTimeout(timer); }
}

// What Claude on the Mac is doing with this session: `busy` while its process
// may be writing a reply (a reply or a dialog), `background` while it keeps
// background work running. Neither when no interactive process has it open.
// `startedAt` is when the earliest open process started, when every record
// says (tasks launched before it ended with an earlier process).
export async function claudeSessionState(sessionId, dir) {
  const scan = await claudeSessionRecords(dir);
  const records = scan?.filter(r => r.sessionId === sessionId);
  if (records == null || records.some(r => r.uncertain)) return { open: null, busy: null, busySince: null, background: null, startedAt: null };
  const starts = records.map(r => r.startedAt);
  const busy = records.filter(r => ["busy", "waiting"].includes(r.status));
  // When the latest reply or dialog there started, when its record says.
  const busySince = busy.map(r => r.statusAt).filter(Number.isSafeInteger);
  return { open: records.length > 0, busy: busy.length > 0 ? true : records.some(r => r.status == null) ? null : false, busySince: busySince.length ? Math.max(...busySince) : null,
    background: records.some(r => ["busy", "shell", "waiting"].includes(r.status)),
    startedAt: starts.length && starts.every(Number.isSafeInteger) ? Math.min(...starts) : null };
}

export async function claudeSessionBusy(sessionId, dir) {
  return (await claudeSessionState(sessionId, dir)).busy;
}

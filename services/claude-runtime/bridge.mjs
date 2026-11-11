import { ByteCache, retainedBytes, historyFingerprint } from "./cooperative.mjs";
import { isProjectTool } from "./thread-tools.mjs";
import { ReadWorkers } from "./read-workers.mjs";
import { open } from "node:fs/promises";
import { constants as fsConstants } from "node:fs";
import { createHash, randomUUID } from "node:crypto";
import { z } from "zod";
import { BRIDGE_PROTOCOL, ClaudeTaskList, HAIKU_MODEL, TurnProjection, nativeContextUsage, questionRequest, questionAnswer } from "./projection.mjs";
import { baseOptions, inspectSdk, accessKind } from "./sdk-runtime.mjs";
import { PromptSuggestions } from "./prompt-suggestions.mjs";
import { SessionCommands, unavailableCommands } from "./session-commands.mjs";
import { NESTED_SANDBOX_GUIDANCE, SANDBOX_GUIDANCE, WINDOWS_COMMAND_GUIDANCE, ToolPolicy, isCommandTool, toolCallKey } from "./permissions.mjs";
import { createCuaDriver } from "./cua-driver.mjs";
import { claudeSessionBusy, claudeSessionState, mainTurnRunning, nativeTurnsAsync, backgroundTasksAsync, rewindPoint, sessionSummary, taskNotification, turnEndMessageId, wakeItem } from "./project-history.mjs";
import { ClaudeSessionFiles, appendedStamp, projectFolderName } from "./session-file.mjs";
import { RUNNING_REFUSAL, STALE_TURN, InputRefusal, resetRefusal } from "./sessions.mjs";
import { lstat, readdir, readFile, realpath, stat } from "node:fs/promises";
import { basename, dirname, isAbsolute, join, relative, resolve, sep } from "node:path";
import { homedir } from "node:os";

const METADATA_READS = new Set(["getSessionInfo", "listSubagents"]);
// A transcript that only grew during a history read (Claude writing the next
// step of a reply) is read again instead of failing the request. History
// retries its projection around the messages read's own retries.
const APPEND_ATTEMPTS = 3;
const UPDATING = "Claude is updating this conversation. Try again in a moment.";
const changed = (before, after) => Object.assign(new Error(UPDATING), { appended: appendedStamp(before, after) });
async function retryingAppends(read) {
  for (let attempt = 1; ; attempt++) {
    try { return await read(); } catch (error) { if (!error.appended || attempt >= APPEND_ATTEMPTS) throw error; }
  }
}

const BUILTINS = ["Read", "Write", "Edit", "NotebookEdit", "Bash", ...(process.platform === "win32" ? ["PowerShell"] : []), "WebFetch", "WebSearch",
  "AskUserQuestion", "Agent", "ToolSearch", "Skill", "TodoWrite", "TaskCreate", "TaskUpdate", "TaskGet", "TaskList", "TaskOutput", "TaskStop"];

// A project conversation is an owner's normal Claude Code session in one of
// their folders with Claude Code settings and verified Wonder thread tools.
async function projectContext(value) {
  if (!value || typeof value !== "object") throw new Error("A Project is required for this session.");
  const { cwd, additionalDirectories = [] } = value;
  if (!isAbsolute(cwd ?? "") || !Array.isArray(additionalDirectories) || additionalDirectories.length > 15
    || additionalDirectories.some(d => typeof d !== "string" || !isAbsolute(d) || d === cwd))
    throw new Error("The project folders are invalid. Edit the project in Cupola.");
  for (const dir of [cwd, ...additionalDirectories]) {
    const info = await stat(dir).catch(() => null);
    if (!info?.isDirectory()) throw new Error("A project folder is missing on this computer. Edit the project in Cupola.");
  }
  const { trustedConfigurations = [] } = value;
  if (!Array.isArray(trustedConfigurations) || trustedConfigurations.length > 16
    || trustedConfigurations.some(f => typeof f !== "string" || !/^[0-9a-f]{64}$/.test(f)))
    throw new Error("The project's trusted Claude settings are invalid. Trust the folder again in Cupola.");
  return { cwd, additionalDirectories: [...new Set(additionalDirectories)], trustedConfigurations: [...new Set(trustedConfigurations)] };
}

// A folder's own Claude Code configuration can run commands by itself (hooks,
// MCP servers, helpers) and loosen what Claude may do without asking, and SDK
// sessions treat every folder as trusted. So a Project's settings, hooks and
// MCP servers load only when the owner trusted exactly what is there now. The
// fingerprint covers each folder's resolved location, the folders above it up
// to its checkout, a linked worktree's main checkout, every file under their
// `.claude`, `.mcp.json`, and the files their commands and settings name
// inside those folders. Anything else runs with the owner's own settings only
// and no MCP servers but Cupola's. A folder with none of this has nothing to
// trust. Commands that reach other files indirectly (a package script, a
// file an agent definition names) are not followed.
const FOLDER_CONFIG = [".claude/settings.json", ".claude/settings.local.json", ".mcp.json"];
const FOLDER_LIMITS = { files: 2000, bytes: 16 * 1024 * 1024 };
const FOLDER_HELPERS = { apiKeyHelper: "to sign in", awsAuthRefresh: "to sign in", awsCredentialExport: "to sign in", otelHeadersHelper: "for usage reports" };
const HOOK_MOMENTS = { SessionStart: "when a chat starts", SessionEnd: "when a chat ends", UserPromptSubmit: "when you send a message",
  PreToolUse: "before Claude uses a tool", PostToolUse: "after Claude uses a tool", Stop: "when Claude finishes a reply",
  SubagentStop: "when a helper agent finishes", Notification: "when Claude notifies you", PreCompact: "before the conversation is summarized" };

async function folderCheckouts(root, home) {
  // A folder, the folders above it up to the top of its checkout (outside a
  // checkout, just the folder; the home folder holds the owner's own
  // settings), and a linked worktree's main checkout.
  const dirs = [];
  for (let dir = root; dir !== home && dir !== dirname(dir); dir = dirname(dir)) {
    dirs.push(dir);
    const git = await lstat(join(dir, ".git")).catch(() => null);
    if (!git) continue;
    if (git.isFile()) {
      const pointer = (await readFile(join(dir, ".git"), "utf8").catch(() => "")).match(/^gitdir:\s*(.+?)\s*$/m)?.[1];
      const admin = pointer && resolve(dir, pointer);
      const common = admin && (await readFile(join(admin, "commondir"), "utf8").catch(() => "")).trim();
      const main = common && await realpath(resolve(admin, common)).catch(() => null);
      if (main && basename(main) === ".git") dirs.push(dirname(main));
    }
    return dirs;
  }
  return root === home ? [] : [root];
}

function commandPaths(text) {
  return [...String(text).matchAll(/"([^"]*)"|'([^']*)'|(\S+)/g)].flatMap(m => (m[1] ?? m[2] ?? m[3]).split(/[=;&|()<>]/)).filter(Boolean);
}

// Commands that run what a file in the folder says rather than the file
// itself: the file is part of what the owner trusts. Scripts a package.json
// names are followed one level.
const FOLDER_RUNNERS = { npm: ["package.json"], npx: ["package.json"], pnpm: ["package.json"], yarn: ["package.json"],
  bun: ["package.json"], node: ["package.json"], make: ["Makefile", "makefile", "GNUmakefile"],
  just: ["justfile", "Justfile", ".justfile"], uv: ["pyproject.toml"], poetry: ["pyproject.toml"], cargo: ["Cargo.toml"],
  deno: ["deno.json", "deno.jsonc"] };

function folderItems(file, parsed) {
  const items = [], quote = text => `“${String(text).length > 200 ? `${String(text).slice(0, 199)}…` : text}”`;
  const commands = [];
  if (file.endsWith(".mcp.json")) {
    for (const [name, server] of Object.entries(parsed?.mcpServers ?? {})) {
      const run = [server?.command, ...(Array.isArray(server?.args) ? server.args : [])].filter(v => typeof v === "string");
      items.push({ kind: "server", text: run.length ? `Starts the tool server ${quote(name)} with ${quote(run.join(" "))}` : `Connects to the tool server ${quote(name)}` });
      // MCP launches a command with structured argv, not shell text. Preserve
      // each path verbatim, including spaces and shell punctuation. Also
      // inspect embedded command strings for servers launched through a shell.
      commands.push([...run, ...run.flatMap(commandPaths)], ...Object.values(server?.env ?? {}).filter(v => typeof v === "string").map(commandPaths));
    }
    return { items, commands };
  }
  for (const [event, groups] of Object.entries(parsed?.hooks ?? {})) for (const group of Array.isArray(groups) ? groups : [])
    for (const hook of Array.isArray(group?.hooks) ? group.hooks : []) if (typeof hook?.command === "string") {
      items.push({ kind: "command", text: `Runs ${quote(hook.command)} ${HOOK_MOMENTS[event] ?? `on ${event}`}` });
      commands.push(hook.command);
    }
  for (const [key, purpose] of Object.entries(FOLDER_HELPERS)) if (typeof parsed?.[key] === "string") {
    items.push({ kind: "command", text: `Runs ${quote(parsed[key])} ${purpose}` }); commands.push(parsed[key]);
  }
  if (typeof parsed?.statusLine?.command === "string") {
    items.push({ kind: "command", text: `Runs ${quote(parsed.statusLine.command)} to show a status line` }); commands.push(parsed.statusLine.command);
  }
  for (const rule of Array.isArray(parsed?.permissions?.allow) ? parsed.permissions.allow : [])
    if (typeof rule === "string") items.push({ kind: "permission", text: `Lets Claude use ${quote(rule)} without asking` });
  if (parsed?.permissions?.defaultMode) items.push({ kind: "permission", text: `Changes how Claude asks for approval (${quote(parsed.permissions.defaultMode)})` });
  if (parsed?.enableAllProjectMcpServers === true) items.push({ kind: "server", text: "Turns on every tool server this folder lists" });
  const servers = Array.isArray(parsed?.enabledMcpjsonServers) ? parsed.enabledMcpjsonServers.filter(s => typeof s === "string") : [];
  if (servers.length) items.push({ kind: "server", text: `Turns on the tool servers ${servers.map(quote).join(", ")}` });
  const plugins = Object.keys(parsed?.enabledPlugins ?? {});
  if (plugins.length) items.push({ kind: "setting", text: `Turns on the plugins ${plugins.map(quote).join(", ")}` });
  const env = Object.entries(parsed?.env ?? {});
  if (env.length) { items.push({ kind: "setting", text: `Sets ${env.map(([k]) => quote(k)).join(", ")} for Claude's commands` }); commands.push(...env.map(([, v]) => v)); }
  return { items, commands: commands.filter(c => typeof c === "string").map(commandPaths) };
}

export async function folderConfiguration({ cwd, additionalDirectories = [] }, home = homedir()) {
  const roots = [...new Set(await Promise.all([cwd, ...additionalDirectories].map(d => realpath(d).catch(() => resolve(d)))))];
  const realHome = await realpath(home).catch(() => resolve(home));
  const dirs = [...new Set((await Promise.all(roots.map(r => folderCheckouts(r, realHome)))).flat())];
  const entries = [], items = [], folders = new Set(), hashed = new Set();
  let servers = false;
  let files = 0, bytes = 0, tooLarge = false;
  const within = path => dirs.some(d => path === d || path.startsWith(`${d}/`));
  const hashFile = async (path, label) => {
    if (hashed.has(path) || tooLarge) return null;
    hashed.add(path);
    const info = await stat(path).catch(() => null);
    if (!info?.isFile()) return null;
    if (++files > FOLDER_LIMITS.files || (bytes += info.size) > FOLDER_LIMITS.bytes) { tooLarge = true; return null; }
    const content = await readFile(path).catch(() => null);
    if (!content) return null;
    entries.push(`${label}\0${path}\0${createHash("sha256").update(content).digest("hex")}`);
    return content;
  };
  const walked = new Set();
  const walk = async (dir, depth) => {
    if (depth > 12 || tooLarge) return;
    for (const entry of (await readdir(dir, { withFileTypes: true }).catch(() => [])).sort((a, b) => a.name < b.name ? -1 : 1)) {
      const path = join(dir, entry.name);
      // Claude Code's own worktrees under .claude are folders of their own.
      if (depth === 0 && entry.name === "worktrees") continue;
      if (entry.isDirectory()) await walk(path, depth + 1);
      else if (entry.isSymbolicLink()) {
        const target = await realpath(path).catch(() => null);
        entries.push(`link\0${path}\0${target ?? "missing"}`);
        // A linked folder's files are this folder's files; each is walked once.
        if (target && (await stat(target).catch(() => null))?.isDirectory()) {
          if (!walked.has(target)) { walked.add(target); await walk(target, depth + 1); }
        } else if (target) await hashFile(target, "linked");
      } else if (entry.isFile()) await hashFile(path, "file");
    }
  };
  const named = [];
  for (const dir of dirs) {
    for (const name of FOLDER_CONFIG) {
      const path = join(dir, name), target = await realpath(path).catch(() => null);
      if (!target) continue;
      folders.add(dir);
      if (target !== path) entries.push(`link\0${path}\0${target}`);
      if (name === ".mcp.json") servers = true;
      const content = await hashFile(target, "config");
      if (!content) continue;
      let parsed = null;
      try { parsed = JSON.parse(content.toString("utf8")); } catch { items.push({ kind: "setting", text: `Includes ${name}, which Claude Code can't read` }); }
      const found = folderItems(name, parsed);
      items.push(...found.items);
      named.push(...found.commands.map(pieces => ({ pieces, dir })));
    }
    const claude = join(dir, ".claude");
    if ((await lstat(claude).catch(() => null))?.isDirectory()) {
      const before = entries.length;
      await walk(claude, 0);
      if (entries.length > before) folders.add(dir);
    }
  }
  // The scripts and files the configuration names, as they are now.
  // A word counts wherever it names a file in these folders: from the
  // configuration's folder, the chat's folder, or a folder the command
  // names first (`cd tools && ./run.sh`).
  for (let i = 0; i < named.length && i < 500; i++) {
    const { pieces: words, dir } = named[i];
    const pieces = words.map(piece => piece.replace(/\$\{?CLAUDE_PROJECT_DIR\}?/g, roots[0]));
    const bases = [dir, roots[0]];
    for (const piece of pieces) {
      for (const base of [...bases]) {
        const target = await realpath(resolve(base, piece)).catch(() => null);
        if (!target || !within(target)) continue;
        const info = await stat(target).catch(() => null);
        if (info?.isDirectory()) { if (!bases.includes(target)) bases.push(target); }
        else if (info?.isFile()) await hashFile(target, "named");
      }
      for (const manifest of FOLDER_RUNNERS[basename(piece)] ?? []) for (const base of bases) {
        const path = join(base, manifest), content = await hashFile(path, "named");
        if (!content || manifest !== "package.json") continue;
        try {
          const { scripts } = JSON.parse(content.toString("utf8"));
          for (const script of Object.values(scripts ?? {})) if (typeof script === "string") named.push({ pieces: commandPaths(script), dir: base });
        } catch {}
      }
    }
  }
  const own = entries.filter(e => !e.startsWith("named\0"));
  const otherFiles = own.filter(e => e.startsWith("file\0") || e.startsWith("linked\0")).length;
  if (otherFiles) items.push({ kind: "files", text: otherFiles === 1 ? "Adds 1 file of its own Claude skills, commands, agents or hooks" : `Adds ${otherFiles} files of its own Claude skills, commands, agents or hooks` });
  if (!entries.length && !tooLarge) return { fingerprint: null, empty: true, tooLarge: false, servers: false, items: [], folders: [] };
  const fingerprint = tooLarge ? null : createHash("sha256").update(["cupola-claude-folder-v1", ...roots.map(r => `root\0${r}`), ...entries].sort().join("\n")).digest("hex");
  return { fingerprint, empty: false, tooLarge, servers: servers || tooLarge, items, folders: [...folders] };
}

// Whether a fresh Claude process may load this folder's own configuration.
export function folderTrusted(configuration, trustedConfigurations = []) {
  return configuration.empty || (configuration.fingerprint != null && trustedConfigurations.includes(configuration.fingerprint));
}

// Wonder's own skill pack, present on a turn only while the owner has one of
// its skills on: a Wonder-owned local plugin, loaded for this process only, with
// only its listed skills enabled. Absent, the session is exactly as before.
export function wonderSkills(options) {
  const value = options.wonderSkills;
  if (value == null) return null;
  if (typeof value !== "object" || !isAbsolute(value.plugin ?? "") || !Array.isArray(value.skills) || !value.skills.length
    || value.skills.length > 8 || value.skills.some(name => typeof name !== "string" || !/^wonder:[a-z0-9-]{1,64}$/.test(name)))
    throw new Error("Cupola's skills are invalid. Turn them off and on again in Settings.");
  return { plugin: value.plugin, skills: [...new Set(value.skills)].sort() };
}

// The owner's custom instruction for Claude, present on a turn only while it
// is on in Wonder's settings. It is appended to Claude Code's system prompt for
// the process that runs the turn; absent, the prompt is exactly as before.
export const INSTRUCTION_MAX_CHARS = 4000;
export function wonderInstruction(options) {
  const value = options.wonderInstruction;
  if (value == null) return null;
  if (typeof value !== "string" || !value.trim() || [...value].length > INSTRUCTION_MAX_CHARS)
    throw new Error("Your custom instruction for Claude is invalid. Change it in Cupola's settings.");
  return value.trim();
}

// Cupola's preamble (where this conversation runs, and its thread tools),
// whose text wonderd owns. It opens what the bridge appends to Claude Code's
// system prompt for the process that runs the turn.
export const PREAMBLE_MAX_CHARS = 2000;
export function wonderPreamble(options) {
  const value = options.wonderPreamble;
  if (value == null) return null;
  if (typeof value !== "string" || !value.trim() || [...value].length > PREAMBLE_MAX_CHARS)
    throw new Error("Cupola couldn't prepare this message for Claude. Restart Cupola on your computer, then send it again.");
  return value.trim();
}

// What Wonder tells Claude about the host for one turn (the owner wrote it on
// the phone, away from this computer's screen). It goes to Claude as turn
// context from a UserPromptSubmit hook, never into the owner's message, so
// the transcript keeps exactly what they wrote and no process is replaced.
export const TURN_CONTEXT_MAX = 4, TURN_CONTEXT_MAX_CHARS = 1000;
export function wonderContext(params) {
  const value = params.wonderContext;
  if (value == null) return [];
  if (!Array.isArray(value) || value.length > TURN_CONTEXT_MAX
    || value.some(line => typeof line !== "string" || !line.trim() || [...line].length > TURN_CONTEXT_MAX_CHARS))
    throw new Error("Cupola couldn't prepare this message for Claude. Restart Cupola on your computer, then send it again.");
  return value.map(line => line.trim());
}

// Context for the prompts Claude is taking now, each delivered once. A prompt
// Claude Code injects itself (a task notification) leaves it for the owner's.
export function promptContext(live, input) {
  if (input?.source && !["sdk", "user"].includes(input.source)) return {};
  const lines = [...new Set(live.context?.splice(0) ?? [])];
  return lines.length ? { hookSpecificOutput: { hookEventName: "UserPromptSubmit", additionalContext: lines.join("\n\n") } } : {};
}

async function fingerprintOf(data, fingerprints, view) {
  let fingerprint = fingerprints?.get(view);
  if (!fingerprint) {
    fingerprint = await historyFingerprint(data);
    fingerprints?.set(view, fingerprint);
    // Per-turn item views must not accumulate for every turn ever opened.
    if (fingerprints?.size > 8) fingerprints.delete(fingerprints.keys().next().value);
  }
  return fingerprint;
}

async function page(data, params, fingerprints = null, view = null) {
  const fingerprint = await fingerprintOf(data, fingerprints, view);
  const [expected, offsetText] = String(params.cursor ?? `${fingerprint}:0`).split(":");
  const offset = Number(offsetText);
  if (expected !== fingerprint || !Number.isSafeInteger(offset) || offset < 0 || offset > data.length) throw new Error("Claude history changed. Refresh the conversation.");
  const limit = Number.isSafeInteger(params.limit) ? Math.max(1, Math.min(params.limit, 200)) : 50;
  return { data: data.slice(offset, offset + limit), nextCursor: offset + limit < data.length ? `${fingerprint}:${offset + limit}` : null };
}

// An unbounded input queue for one Claude process. Later replies are pushed
// into the same open prompt stream instead of starting a new process.
function channel() {
  const queue = []; let wake = null, closed = false;
  return { push(message) { queue.push(message); wake?.(); }, close() { closed = true; wake?.(); },
    async next() {
      while (!queue.length) { if (closed) return undefined; await new Promise(resolve => { wake = resolve; }); }
      return queue.shift();
    } };
}

// What a running Claude process was started with. A new reply may reuse the
// process only when this matches. Model, effort and permission mode have
// native session controls; other changes need a new process, ending its tasks.
function processShape(options, policy) {
  return JSON.stringify({ schema: options.outputSchema ?? null,
    cwd: options.cwd, dirs: options.wonderProject.additionalDirectories, tools: advertisedTools(options).map(t => t.name),
    plan: policy.planMode, unsandboxed: policy.unsandboxed, sandbox: policy.sandbox(),
    ...(wonderSkills(options) ? { skills: wonderSkills(options) } : {}),
    ...(wonderPreamble(options) ? { preamble: wonderPreamble(options) } : {}),
    ...(wonderInstruction(options) ? { instruction: wonderInstruction(options) } : {}),
    ...(cuaDriver(options) ? { cuaDriver: [cuaDriver(options).command, cuaDriver(options).args, cuaDriver(options).env] } : {}),
    // A process keeps the folder configuration it started with.
    ...(options.folderTrust ? { folder: [options.folderTrust.trusted, options.folderTrust.fingerprint] } : {}) });
}

// The Cua Driver (trycua/cua) the daemon named for this thread, a stdio MCP
// server the bridge runs (see cua-driver.mjs). The
// provider environment is filtered, so the daemon names the X display.
function cuaDriver(options) {
  const driver = options.config?.["mcp_servers.cua_driver"];
  if (driver?.enabled !== true || typeof driver.command !== "string") return null;
  const env = Object.fromEntries(Object.entries(driver.env ?? {}).filter(([, value]) => typeof value === "string"));
  return { command: driver.command, args: Array.isArray(driver.args) ? driver.args : [],
    ...(Object.keys(env).length ? { env } : {}) };
}

// SDK flag settings can change together on the retained process. Keep its
// allowed model in sync with the choice, including clearing a previous effort.
// The choice's effort "ultracode" is Claude Code's ultracode setting (standing
// dynamic-workflow orchestration) on top of its highest effort; speed "fast" is
// its fast mode. Everything else is the SDK's own effort level.
const ULTRACODE = "ultracode";
const sdkEffort = options => (options.effort === ULTRACODE ? "max" : options.effort ?? null);
const choiceKey = options => `${options.effort ?? ""}|${options.serviceTier === "fast" ? "fast" : ""}`;

function modelSettings(options, clearDefaults = true) {
  const model = selectedModel(options.model);
  const settings = { model, availableModels: [model], effortLevel: sdkEffort(options),
    fastMode: options.serviceTier === "fast", ultracode: options.effort === ULTRACODE,
    alwaysThinkingEnabled: model === HAIKU_MODEL ? false : null };
  // Startup effort uses the SDK option, whose type also accepts max. The
  // live control accepts max and null for resetting an earlier selection.
  if (!clearDefaults) {
    delete settings.effortLevel;
    if (settings.alwaysThinkingEnabled === null) delete settings.alwaysThinkingEnabled;
  }
  return settings;
}

// How long a full inspection answers routine reads (see catalog()).
const INSPECTION_FRESH_MS = 30 * 60_000;

// A Project conversation keeps its Claude process between replies, so the
// next message skips process start and session resume. An idle one ends
// after WARM_IDLE_MS, or sooner when more than MAX_WARM are idle; one kept
// for background agents, commands or monitors ends after BACKGROUND_HOLD_MS.
const WARM_IDLE_MS = 10 * 60_000;
const MAX_WARM = 4;
const BACKGROUND_HOLD_MS = 6 * 60 * 60_000;
// Where Claude Code writes background command output (see taskOutput).
const CLAUDE_TASKS_ROOT = `/private/tmp/claude-${process.getuid?.() ?? 0}`;
// Claude Code's task list files (TaskCreate/TaskUpdate), one folder per session.
const claudeTaskListsDir = () => join(process.env.CLAUDE_CONFIG_DIR ?? join(homedir(), ".claude"), "tasks");
// While the owner controls the computer from a phone, Claude's computer use
// waits (see the daemon's wonder_computer_control_check).
export const OWNER_HAS_CONTROL = "The owner has control; wait.";
export const CONTROL_UNKNOWN = "Cupola couldn't check whether the owner is using this computer, so this action did not run. Try again shortly.";
const CONTROL_CHECK = "wonder_computer_control_check";
const BACKGROUND_REFUSAL = "Claude is still running background tasks in this conversation. Changing access, tools or folders now would stop them. Wait for them to finish or stop them, then send again.";

export function selectedModel(value) {
  // A saved Haiku 4.5 choice keeps running Haiku 4.5; the "haiku" alias follows
  // Claude Code to its current Haiku.
  if (value === HAIKU_MODEL) return HAIKU_MODEL;
  if (typeof value !== "string" || !/^claude:[a-zA-Z0-9._\[\]-]{1,100}$/.test(value)) throw new Error("Choose a Claude model for this chat.");
  const model = value.slice("claude:".length);
  if (model === "default") throw new Error("Choose an explicit Claude model.");
  return model;
}

function modelVersion(model) {
  const resolved = model.resolvedModel ?? model.id;
  const match = /^claude-([a-z]+)-(\d{1,2})(?:-(\d{1,2}))?(?:-\d{8})?(?:\[1m\])?$/.exec(resolved);
  if (!match) return null;
  const [, family, major, minor] = match;
  return { family, major: Number(major), minor: Number(minor ?? 0), minorText: minor };
}

function modelDisplayName(model) {
  // Keep selection aliases stable, but label the model they resolve to today.
  const version = modelVersion(model);
  if (!version) return model.name;
  const { family, major, minorText } = version;
  return `${family[0].toUpperCase()}${family.slice(1)} ${major}${minorText ? `.${minorText}` : ""}`;
}

// Claude Code's catalog in its own order (Opus, Fable, Sonnet, Haiku), keeping
// only the newest version of each family: an older pinned entry such as
// Haiku 4.5 beside the current Haiku is not offered.
export function offeredModels(models) {
  const listed = models.filter(m => m.id !== "default");
  const newest = new Map();
  for (const model of listed) {
    const version = modelVersion(model);
    if (!version) continue;
    const best = newest.get(version.family);
    if (!best || version.major > best.major || (version.major === best.major && version.minor > best.minor)) newest.set(version.family, version);
  }
  return listed.filter(model => {
    const version = modelVersion(model);
    if (!version) return true;
    const best = newest.get(version.family);
    return version.major === best.major && version.minor === best.minor;
  });
}

// A Project turn offers the agent only Wonder's thread tools (thread-tools.mjs):
// the daemon sends them with the turn and their origin is verified before execution.
function advertisedTools(options) {
  const specs = options.dynamicTools ?? [];
  return specs.filter(spec => isProjectTool(spec.name));
}

function sdkToolResult(result) {
  const content = (result?.contentItems ?? []).flatMap(block => {
    if (block.type === "inputText") return [{ type: "text", text: block.text }];
    if (block.type === "inputImage") {
      const image = /^data:(image\/(?:png|jpeg|webp|gif));base64,([A-Za-z0-9+/=]+)$/.exec(block.imageUrl ?? "");
      if (image) return [{ type: "image", mimeType: image[1], data: image[2] }];
    }
    return [];
  });
  return { content: content.length ? content : [{ type: "text", text: "The action returned no displayable result." }], isError: result?.success !== true };
}

// Wonder's authenticated attachment files use opaque IDs without an
// extension. Match their bytes rather than trusting a filename suffix.
function sdkImage(bytes) {
  const mime = bytes.subarray(0, 8).equals(Buffer.from([137,80,78,71,13,10,26,10])) ? "image/png"
    : bytes[0] === 255 && bytes[1] === 216 && bytes[2] === 255 ? "image/jpeg"
    : ["GIF87a", "GIF89a"].includes(bytes.subarray(0, 6).toString()) ? "image/gif"
    : bytes.subarray(0, 4).toString() === "RIFF" && bytes.subarray(8, 12).toString() === "WEBP" ? "image/webp" : null;
  if (!mime) throw new Error("This image format is not supported by Claude.");
  return { type: "image", source: { type: "base64", media_type: mime, data: bytes.toString("base64") } };
}

async function sdkInput(input, policy) {
  if (!Array.isArray(input) || !input.length) throw new Error("Claude requires a message.");
  const content = [];
  for (const block of input) {
    if (block.type === "text" && typeof block.text === "string") content.push({ type: "text", text: block.text });
    else if (block.type === "localImage") {
      if (!await policy.permits(block.path)) throw new Error("The attached image is outside the files this chat can use.");
      const file = await open(block.path, "r");
      let bytes;
      try {
        const stat = await file.stat();
        if (!stat.isFile() || stat.size > 20 * 1024 * 1024) throw new Error("The attached image is too large for Claude.");
        if (!await policy.permits(block.path)) throw new Error("The attached image's location changed.");
        bytes = Buffer.alloc(stat.size);
        let count = 0;
        while (count < bytes.length) {
          const { bytesRead } = await file.read(bytes, count, bytes.length - count, count);
          if (!bytesRead) throw new Error("The attached image changed while it was being read.");
          count += bytesRead;
        }
        if ((await file.stat()).size !== stat.size) throw new Error("The attached image changed while it was being read.");
      } finally { await file.close(); }
      content.push(sdkImage(bytes));
    } else if (block.type === "image") {
      // Wonder sends an attachment as the bytes it verified, so nothing
      // rereads a file that may have changed since it was checked.
      const data = /^data:image\/[a-z0-9.+-]+;base64,([A-Za-z0-9+/=]+)$/.exec(block.url ?? "")?.[1];
      if (!data) throw new Error("This image format is not supported by Claude.");
      if (data.length > Math.ceil(20 * 1024 * 1024 / 3) * 4) throw new Error("The attached image is too large for Claude.");
      content.push(sdkImage(Buffer.from(data, "base64")));
    } else throw new Error("This attachment type is not supported by Claude. Dictation can send its transcript as text.");
  }
  // A typed `/name` runs that skill or command with the tools it names.
  const typed = /^\s*\/([^\s/]+)/.exec(content.find(block => block.type === "text")?.text ?? "");
  if (typed) policy.noteSkill(typed[1]);
  return content;
}

// Claude Code reports the session's live background tasks (agents, commands,
// monitors and others) as one list whenever it changes. It is the same roster
// Claude on the Mac counts, so a Wonder-driven session uses it directly.
function observeTasks(roster, message, sessionId) {
  if (!roster || message?.type !== "system" || (message.session_id && message.session_id !== sessionId)) return;
  if (message.subtype === "background_tasks_changed" && Array.isArray(message.tasks)) {
    const tasks = new Map(message.tasks.slice(0, 100)
      .filter(t => typeof t?.task_id === "string" && t.task_id.length > 0 && t.task_id.length <= 256)
      .map(t => [t.task_id, t]));
    // A task removed from Claude's live roster cannot keep showing running copy.
    for (const [id, update] of roster.updates ?? []) {
      if (update.status === "running" && roster.tasks?.has(id) && !tasks.has(id))
        roster.updates.set(id, { status: "unknown", progressSummary: null });
    }
    roster.tasks = tasks;
    return;
  }
  const id = message.task_id;
  if (typeof id !== "string" || !id || id.length > 256 || message.ambient || message.skip_transcript) return;
  const tool = message.tool_use_id;
  if (tool != null && (typeof tool !== "string" || !tool || tool.length > 256)) return;
  const updates = roster.updates ??= new Map();
  if (message.subtype === "task_started") {
    if (tool) roster.toolUses.set(id, tool);
    updates.set(id, { status: "running", progressSummary: null });
  } else {
    // A progress event does not create a task or move it to another tool call.
    if ((!updates.has(id) && !roster.tasks?.has(id)) || (tool && roster.toolUses.has(id) && roster.toolUses.get(id) !== tool)) return;
    if (message.subtype === "task_notification") {
      const status = { completed: "completed", failed: "failed", stopped: "interrupted" }[message.status];
      if (status) updates.set(id, { status, progressSummary: null });
    } else if (message.subtype === "task_progress" && (!updates.has(id) || updates.get(id).status === "running")) {
      const summary = typeof message.summary === "string"
        ? [...message.summary.slice(0, 2048).replace(/[\uD800-\uDFFF]/gu, "")
          .replace(/[\s\u0000-\u001f\u007f]+/g, " ").trim()].slice(0, 240).join("") : "";
      // The SDK also sends usage-only progress between its periodic summaries.
      // Only a valid summary-bearing event replaces the last summary.
      if (summary) updates.set(id, { status: "running", progressSummary: summary });
    }
  }
  while (updates.size > 100) updates.delete(updates.keys().next().value);
  while (roster.toolUses.size > 100) roster.toolUses.delete(roster.toolUses.keys().next().value);
}

// A process that exits can send no more notices. Background items its turns
// left running take the outcome the transcript recorded, else interrupted.
function settleEndedBackground(projection, tasks, handle) {
  let changed = false;
  for (const task of tasks) {
    if (["completed", "failed", "interrupted"].includes(task.status))
      changed = projection.settleBackground(task.id, { status: task.status, error: task.summary }) || changed;
  }
  const threads = [projection.threadId, ...[...(handle.children?.values() ?? [])].map(child => child.session.id)];
  return projection.settleAllBackground("interrupted", undefined, threads) || changed;
}

const rosterKind = type => ["local_agent", "remote_agent", "in_process_teammate"].includes(type) ? "agent"
  : type === "local_bash" ? "command" : String(type ?? "").startsWith("monitor") ? "monitor" : "task";

// Running tasks the transcript cannot name yet (or never records, such as an
// MCP task) come from the live roster; a listed task is running.
export function withLiveRoster(tasks, roster) {
  if (!roster?.tasks && !roster?.updates?.size) return tasks;
  const result = tasks.map(task => {
    const taskId = task.taskId ?? [...roster.toolUses].find(([, toolUse]) => toolUse === task.id)?.[0] ?? null;
    return taskId && roster.tasks?.has(taskId) ? { ...task, taskId, status: "running" } : taskId ? { ...task, taskId } : task;
  });
  for (const [taskId, live] of roster.tasks ?? []) {
    if (result.some(task => task.taskId === taskId)) continue;
    const id = roster.toolUses.get(taskId) ?? `task:${taskId}`;
    const index = result.findIndex(task => task.id === id);
    if (index >= 0) { result[index] = { ...result[index], taskId, status: "running" }; continue; }
    const kind = rosterKind(live.task_type);
    result.unshift({ id, kind, background: true, title: typeof live.description === "string" && live.description.trim()
        ? live.description.trim().slice(0, 200) : kind === "agent" ? "Agent task" : "Background task",
      role: null, status: "running", request: null, summary: null, result: null, outputFile: null, taskId, startedAt: null, updatedAt: null });
  }
  return result.map(task => {
    const update = roster.updates?.get(task.taskId);
    const status = !update || update.status === "running" || (update.status === "unknown" && task.status !== "running")
      ? task.status : update.status;
    return { ...task, status, progressSummary: status === "running" && update?.status === "running" ? update.progressSummary : null };
  });
}

// An edit approval shows the change Claude proposes against the file as it is
// now: hunks with three lines of context, or a new file's content. A diff over
// the cap is left out and marked, never cut short, so the owner never approves
// part of what they saw. An edit that does not apply to the file shows the
// text Claude asked to replace, without line numbers.
const EDIT_TOOLS = new Set(["Edit", "MultiEdit", "Write"]);
export const PROPOSED_DIFF_BYTES = 64 * 1024;
const PROPOSED_FILE_BYTES = 8 * 1024 * 1024;
const DIFF_CONTEXT = 3;
// A line lacking its final newline compares and prints apart from one with it.
const NO_EOL = "\u0000";
function diffLines(text) {
  if (text === "") return [];
  const lines = text.split("\n");
  if (lines.at(-1) === "") lines.pop(); else lines[lines.length - 1] += NO_EOL;
  return lines;
}
// Myers' shortest edit script for the lines between a common prefix and
// suffix. Past its bounds the middle is shown as wholly replaced: larger, but
// still exactly the proposed change.
function editScript(a, b) {
  const n = a.length, m = b.length, max = n + m, offset = max + 1;
  const replaced = () => [...a.map(line => ["-", line]), ...b.map(line => ["+", line])];
  if (!n || !m || max > 20_000) return replaced();
  const v = new Int32Array(2 * max + 3), trace = [];
  for (let d = 0; d <= Math.min(max, 2_000); d++) {
    trace.push(v.slice(offset - d - 1, offset + d + 2));
    for (let k = -d; k <= d; k += 2) {
      let x = k === -d || (k !== d && v[offset + k - 1] < v[offset + k + 1]) ? v[offset + k + 1] : v[offset + k - 1] + 1;
      let y = x - k;
      while (x < n && y < m && a[x] === b[y]) { x++; y++; }
      v[offset + k] = x;
      if (x >= n && y >= m) return backtrack(trace, a, b);
    }
  }
  return replaced();
}
function backtrack(trace, a, b) {
  const ops = [];
  let x = a.length, y = b.length;
  for (let d = trace.length - 1; d >= 0; d--) {
    const at = k => trace[d][k + d + 1], k = x - y;
    const previous = k === -d || (k !== d && at(k - 1) < at(k + 1)) ? k + 1 : k - 1;
    const px = at(previous), py = px - previous;
    while (x > px && y > py) { x--; y--; ops.push(["=", a[x]]); }
    if (d > 0) ops.push(x === px ? ["+", b[--y]] : ["-", a[--x]]);
    x = px; y = py;
  }
  return ops.reverse();
}
export function unifiedDiff(before, after) {
  const a = diffLines(before), b = diffLines(after);
  let start = 0, endA = a.length, endB = b.length;
  while (start < endA && start < endB && a[start] === b[start]) start++;
  while (endA > start && endB > start && a[endA - 1] === b[endB - 1]) { endA--; endB--; }
  const ops = [...a.slice(0, start).map(line => ["=", line]), ...editScript(a.slice(start, endA), b.slice(start, endB)),
    ...a.slice(endA).map(line => ["=", line])];
  const groups = [];
  ops.forEach(([type], index) => {
    if (type === "=") return;
    const last = groups.at(-1);
    if (last && index - last.end <= 2 * DIFF_CONTEXT + 1) last.end = index; else groups.push({ start: index, end: index });
  });
  const oldAt = [], newAt = [];
  let oldLine = 1, newLine = 1;
  for (const [type] of ops) { oldAt.push(oldLine); newAt.push(newLine); if (type !== "+") oldLine++; if (type !== "-") newLine++; }
  return groups.map(({ start, end }) => {
    const from = Math.max(0, start - DIFF_CONTEXT), hunk = ops.slice(from, end + DIFF_CONTEXT + 1);
    const oldCount = hunk.filter(([type]) => type !== "+").length, newCount = hunk.filter(([type]) => type !== "-").length;
    const range = (line, count) => `${count ? line : line - 1},${count}`;
    return [`@@ -${range(oldAt[from], oldCount)} +${range(newAt[from], newCount)} @@`, ...hunk.map(([type, line]) =>
      `${type === "=" ? " " : type}${line.endsWith(NO_EOL) ? `${line.slice(0, -1)}\n\\ No newline at end of file` : line}`)].join("\n");
  }).join("\n");
}
// Approval previews are an extra disclosure, not a Read tool call. Full access
// to edit a file must not silently copy its existing secrets to the phone and
// approval history. Only ordinary Project files get contextual previews. Check
// both spellings so links cannot import hidden files or files outside the folder.
function previewableFile(path, root) {
  const local = relative(root, path);
  return local && !isAbsolute(local) && !local.split(sep).some(part => part.startsWith("."))
    && !/^(?:credentials(?:\..*)?|secrets?(?:\..*)?|id_(?:rsa|dsa|ecdsa|ed25519)(?:\..*)?)$/i.test(basename(path))
    && !/\.(?:pem|key|p12|pfx|keystore)$/i.test(path);
}
async function currentFile(path, cwd) {
  if (!isAbsolute(cwd ?? "") || !previewableFile(path, resolve(cwd))) return { unreadable: true };
  try {
    const root = await realpath(cwd), target = await realpath(path);
    if (!previewableFile(target, root)) return { unreadable: true };
    const handle = await open(target, fsConstants.O_RDONLY | fsConstants.O_NOFOLLOW | fsConstants.O_NONBLOCK);
    try {
      const info = await handle.stat();
      if (!info.isFile()) return { unreadable: true };
      if (info.size > PROPOSED_FILE_BYTES) return { tooLarge: true };
      // Bound the read itself as well: the file may grow after stat.
      const buffer = Buffer.alloc(PROPOSED_FILE_BYTES + 1);
      let size = 0;
      while (size < buffer.length) {
        const { bytesRead } = await handle.read(buffer, size, buffer.length - size, null);
        if (!bytesRead) break;
        size += bytesRead;
      }
      return size > PROPOSED_FILE_BYTES ? { tooLarge: true } : { text: buffer.toString("utf8", 0, size) };
    } finally { await handle.close(); }
  } catch (error) { return error?.code === "ENOENT" ? { missing: true } : { unreadable: true }; }
}
// Claude Code's own rules: a unique match unless replace_all, and an empty
// old_string only creates a file.
function applyEdits(text, edits) {
  let result = text;
  for (const { old_string: before, new_string: after, replace_all: all } of edits) {
    if (before === "") { if (result !== null && result !== "") return null; result = after; continue; }
    if (result === null) return null;
    const parts = result.split(before);
    if (parts.length < 2 || (parts.length > 2 && all !== true)) return null;
    result = parts.join(after);
  }
  return result;
}
const literalHunk = ({ old_string: before, new_string: after }) => ["@@ @@",
  ...diffLines(before).map(line => `-${line.replace(NO_EOL, "")}`), ...diffLines(after).map(line => `+${line.replace(NO_EOL, "")}`)].join("\n");
// The change one Edit, MultiEdit or Write call proposes, as `{path, kind, diff}`;
// `truncated` replaces a diff over the cap. Null for any other tool.
export async function proposedChange(name, input, cwd) {
  const raw = typeof input?.file_path === "string" ? input.file_path : "";
  if (!EDIT_TOOLS.has(name) || !raw) return null;
  const path = cwd ? resolve(cwd, raw) : raw;
  const edits = name === "Write" ? null : name === "Edit" ? [input] : Array.isArray(input.edits) ? input.edits : [];
  const valid = edits ? edits.length > 0 && edits.every(e => typeof e?.old_string === "string" && typeof e?.new_string === "string")
    : typeof input.content === "string";
  if (!valid) return { path, kind: "update" };
  const file = await currentFile(path, cwd);
  if (file.unreadable) return { path, kind: "update" };
  if (file.tooLarge) return { path, kind: "update", truncated: true };
  const before = file.missing ? null : file.text ?? undefined;
  const after = edits ? (before === undefined ? null : applyEdits(before, edits)) : input.content;
  const kind = file.missing && after !== null ? "add" : "update";
  const diff = after === null ? edits.map(literalHunk).join("\n")
    : before === undefined ? null : kind === "add" ? after : unifiedDiff(before, after);
  if (diff === null) return { path, kind };
  return Buffer.byteLength(diff, "utf8") > PROPOSED_DIFF_BYTES ? { path, kind, truncated: true } : { path, kind, diff };
}

export class ClaudeBridge {
  constructor({ updates, sessions, send, inspect = inspectSdk, onFatal = () => {}, claudeSessionsDir = undefined, claudeProjectsDir = undefined, yieldCheckMs = 1000,
    warmIdleMs = WARM_IDLE_MS, maxWarm = MAX_WARM, backgroundHoldMs = BACKGROUND_HOLD_MS, claudeTasksRoot = CLAUDE_TASKS_ROOT,
    taskListsDir = claudeTaskListsDir() }) {
    Object.assign(this, { updates, sessions, send, inspect, onFatal, claudeSessionsDir, yieldCheckMs, warmIdleMs, maxWarm, backgroundHoldMs, claudeTasksRoot, taskListsDir });
    this.sessionFiles = new ClaudeSessionFiles(claudeProjectsDir, action => this.withSdk(action));
    // A transcript too large for the budget (about 16 MB of JSONL) is held
    // alone for 30 s of idle time: its history pages and refresh checkpoint
    // reuse one read, and its parse is still released afterwards.
    this.transcripts = new ByteCache(64 * 1024 * 1024, 4, { oversizeMs: 30_000 }); this.inProcessReadBytes = 8 * 1024 * 1024; this.readers = new ReadWorkers(); this.messageReads = new Map(); this.taskMessageReads = new Map(); this.historyEpoch = 0; this.attaching = new Map(); this.resetSaves = new Map(); this.rewinding = new Set();
    // A turn being admitted (saved before its run is registered) per session.
    this.admitting = new Map();
    this.active = new Map(); this.handoffs = new Set(); this.lingering = new Map(); this.taskProjections = new ByteCache(4 * 1024 * 1024, 8); this.pending = new Map(); this.inspection = null; this.inspectionAt = 0;
    sessions.pinned = session => this.active.has(session.id) || this.lingering.has(session.id)
      || this.admitting.has(session.id) || session.turns.some(turn => this.handoffs.has(turn.id)) || this.resetSaves.has(session.id) || this.rewinding.has(session.id)
      || [...this.active.values(), ...this.lingering.values()].some(run => [...(run.children?.values() ?? [])].some(child => child.session === session));
    this.stopActivated = updates.onActivated?.(runtime => this.runtimeActivated(runtime));
  }
  // An activated SDK can map the same aliases to newer models. Drop the old
  // inspection and ask wonderd to rediscover now rather than on its next poll.
  async runtimeActivated(runtime) {
    this.historyEpoch++; this.transcripts.clear(); this.taskProjections.clear(); this.messageReads.clear();
    this.inspection = null; this.inspectionAt = 0;
    await this.send({ method: "runtime/activated", params: { agentFamily: "claude", version: runtime.version } });
  }
  // A full inspection starts Claude Code and reads the account, models, usage
  // and connectors. Routine reads (wonderd's periodic check, model lists)
  // reuse it for half an hour; an explicit refresh, usage, a connector
  // refetch and an activated runtime inspect now.
  async catalog(refresh = false) {
    // Account, model and app requests can arrive together during startup.
    // Share discovery so an older empty response cannot replace a newer list.
    if (this.inspecting) return this.inspecting;
    if (!refresh && this.inspection && Date.now() - this.inspectionAt <= INSPECTION_FRESH_MS) return this.inspection;
    this.inspecting = (async () => {
      const lease = await this.updates.acquire();
      try { this.inspection = await this.inspect(lease.runtime, { includeUsage: true, connectors: true }); this.inspectionAt = Date.now(); return this.inspection; }
      finally { await lease.release(); }
    })();
    try { return await this.inspecting; }
    finally { this.inspecting = null; }
  }
  // Prompt receipts describe what was answered. The retained SDK process can
  // start another main reply afterward; only its own state event says so.
  processHandle(session) { return this.active.get(session.id)?.handle ?? this.lingering.get(session.id); }
  processBusy(handle) { return ["running", "requires_action"].includes(handle?.sessionState); }
  sessionBusy(session) { return this.active.has(session.id) || this.processBusy(this.processHandle(session)); }
  describe(session, includeTurns = false) {
    const thread = this.sessions.describe(session, includeTurns);
    const handle = this.processHandle(session);
    if (this.sessionBusy(session)) {
      // requires_action does not identify which dialog. Only the actual SDK
      // request supplies that distinction; no fabricated approval is created.
      const flags = handle?.sessionState === "requires_action" ? [...this.pending.values()]
        .filter(p => p.params?.threadId === session.id)
        .flatMap(p => p.method === "item/tool/requestUserInput" ? ["waitingOnUserInput"]
          : ["item/commandExecution/requestApproval", "item/fileChange/requestApproval", "mcpServer/elicitation/request"].includes(p.method) ? ["waitingOnApproval"] : []) : [];
      thread.status = { type: "active", activeFlags: [...new Set(flags)] };
    } else if (handle && handle.sessionState == null) {
      // Older/missing SDK events provide no native process level. A completed
      // receipt remains completed, while this process state stays unknown.
      thread.status = { type: "notLoaded", activeFlags: [] };
    }
    return thread;
  }
  // A native reset's display conversation UUID is not its resumable session ID.
  // Persist the transition before acknowledging a native hook. Claude waits for
  // that hook, so daemon acknowledgement happens only after the old turn flushes.
  async recordNativeReset(session, handle, { sessionId, message } = {}) {
    if (!handle || this.processHandle(session) !== handle || handle.abort.signal.aborted) return;
    const valid = id => typeof id === "string" && /^[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}$/i.test(id);
    const fresh = !session.pendingReset;
    const reset = session.pendingReset ??= { oldSessionId: session.sdkSessionId };
    if (fresh) {
      handle.contextRevision = (handle.contextRevision ?? 0) + 1;
      delete handle.contextSnapshot;
      handle.sessionState = null;
    }
    if (message) {
      reset.observed = true;
      reset.frameId = message.uuid ?? null;
      // Only /clear's terminal boundary is established here. Other native
      // resets may split an active reply; do not invent that history contract.
      reset.unsupported = message.trigger != null ? message.trigger !== "clear" : !message.user_message_uuid;
      reset.userMessageUuid = message.user_message_uuid ?? null;
      reset.invalid = !valid(message.new_conversation_id);
    }
    if (valid(sessionId) && sessionId !== reset.oldSessionId) {
      if (reset.newSessionId && reset.newSessionId !== sessionId) reset.invalid = true;
      else reset.newSessionId = sessionId;
    }
    handle.suggestions.invalidate();
    await this.sessions.save(session);
    if (reset.newSessionId && !reset.invalid && !reset.unsupported) handle.commands?.identifyResetSession(reset.newSessionId);
  }
  async observeNativeReset(session, handle, message) {
    if (message.parent_tool_use_id || this.processHandle(session) !== handle) return;
    if (message.type === "conversation_reset") {
      await this.recordNativeReset(session, handle, { message });
    } else if (session.pendingReset && ((message.type === "system" && message.subtype === "init")
      || (message.type === "result" && (!session.pendingReset.userMessageUuid
        || [message.user_message_uuid, ...(message.user_message_uuids ?? [])].includes(session.pendingReset.userMessageUuid))))) {
      await this.recordNativeReset(session, handle, { sessionId: message.session_id });
    }
  }
  async finishNativeReset(session) {
    if (this.resetSaves.has(session.id)) return this.resetSaves.get(session.id);
    const reset = session.pendingReset;
    if (!reset?.newSessionId || !reset.observed || reset.invalid || reset.unsupported || reset.unverified) return;
    const saving = (async () => {
      // Capture accepted Guide ownership durably: restart marks its receipt
      // interrupted, but retrying the reset must retain the same native turns.
      if (!reset.retainedTurnIds) {
        reset.retainedTurnIds = session.turns.filter(turn => turn.status === "inProgress").map(turn => turn.id);
        await this.sessions.save(session);
      }
      const retained = new Set(reset.retainedTurnIds);
      const result = await this.serverCall("thread/session/reset", { threadId: session.id, agentFamily: "claude",
        oldSessionId: reset.oldSessionId, sessionId: reset.newSessionId,
        retainedTurnIds: reset.retainedTurnIds }, AbortSignal.timeout(5000));
      if (result?.sessionId !== reset.newSessionId) throw new Error("Claude's conversation reset could not be saved.");
      if (session.pendingReset !== reset || reset.invalid) throw new Error("Claude's reset identity changed before it was saved.");
      // Save the completed transition before releasing admission. If this write
      // fails, pending intent remains and replaying the daemon ACK is safe.
      for await (const child of this.sessions.childrenOf(session.id)) {
        if (child.parent?.threadId === session.id && !child.parent.nativeSessionId) {
          child.parent.nativeSessionId = reset.oldSessionId;
          await this.sessions.save(child);
        }
      }
      const retainedTurns = session.turns.filter(turn => retained.has(turn.id));
      const saved = structuredClone(session);
      for (const turn of saved.turns) if (turn.clientUserMessageId)
        saved.receipts[turn.clientUserMessageId] = { id: turn.id, status: turn.status === "inProgress" ? "interrupted" : turn.status };
      saved.sdkSessionId = reset.newSessionId; saved.sdkStarted = true;
      saved.turns = saved.turns.filter(turn => retained.has(turn.id));
      delete saved.pendingReset;
      await this.sessions.saveSnapshot(session, saved);
      Object.assign(session, saved); session.turns = retainedTurns; delete session.pendingReset;
      for (const [id, request] of this.pending) {
        if (request.params?.threadId === session.id && request.params?.turnId && !retained.has(request.params.turnId)) {
          this.pending.delete(id); request.cleanup(); request.reject(new Error("Claude reset this conversation; the earlier request has ended."));
        }
      }
      const handle = this.processHandle(session);
      handle?.commands?.identifyResetSession(session.sdkSessionId);
      if (handle) {
        handle.roster = { tasks: null, toolUses: new Map(), startedAt: Date.now() };
        handle.children = new Map(); handle.childTasks = new Map(); handle.childMessages = [];
        handle.followUp = null;
      }
      this.transcripts.delete(reset.oldSessionId); this.taskProjections.delete(session.id);
    })();
    this.resetSaves.set(session.id, saving);
    try { await saving; } finally { this.resetSaves.delete(session.id); }
  }
  observeContextUsage(session, handle, message) {
    if (this.processHandle(session) !== handle || handle.abort.signal.aborted || message.parent_tool_use_id
      || (message.session_id && message.session_id !== session.sdkSessionId)) return;
    if (message.type === "result" && handle.contextSnapshot?.nativeEvent) return;
    if (["user", "assistant", "result"].includes(message.type)
      || (message.type === "system" && ["compact_boundary", "session_state_changed"].includes(message.subtype))) {
      handle.contextRevision = (handle.contextRevision ?? 0) + 1;
      delete handle.contextSnapshot;
      if (message.type === "assistant" && Object.hasOwn(message, "context_usage")) {
        const usage = message.context_usage;
        handle.contextSnapshot = { nativeEvent: true, revision: handle.contextRevision, observedAtMs: Date.now(), tokenUsage:
          nativeContextUsage({ totalTokens: usage?.total_tokens, rawMaxTokens: usage?.raw_max_tokens, model: usage?.model }) };
      }
    }
  }
  async contextUsage(session) {
    const handle = this.processHandle(session);
    if (!handle) return undefined;
    if (session.pendingReset) return null;
    const nativeId = session.sdkSessionId;
    const current = () => this.processHandle(session) === handle && !handle.abort.signal.aborted
      && !session.pendingReset && session.sdkSessionId === nativeId;
    if (!current()) return null;
    if (handle.contextSnapshot?.revision === (handle.contextRevision ?? 0)) return handle.contextSnapshot;
    if (typeof handle.query?.getContextUsage !== "function") return undefined;
    if (!handle.contextRead) {
      const revision = handle.contextRevision ?? 0;
      const pending = Promise.resolve().then(() => handle.query.getContextUsage({ detail: "summary" }))
        .then(summary => {
          if (!current() || (handle.contextRevision ?? 0) !== revision) return null;
          return handle.contextSnapshot = { revision, observedAtMs: Date.now(), tokenUsage: nativeContextUsage(summary) };
        }, () => null);
      handle.contextRead = pending;
      // Keep a timed-out operation in flight until the SDK actually settles it.
      pending.finally(() => { if (handle.contextRead === pending) delete handle.contextRead; });
    }
    let timer;
    try {
      const value = await Promise.race([handle.contextRead, new Promise(resolve => { timer = setTimeout(() => resolve(null), 250); })]);
      return current() ? value : null;
    } finally { clearTimeout(timer); }
  }
  async publishSessionStatus(session, handle) {
    if (handle?.sessionState == null) return;
    const status = this.describe(session).status;
    const key = JSON.stringify(status);
    if (handle?.publishedStatus === key) return;
    if (handle) handle.publishedStatus = key;
    await this.send({ method: "thread/status/changed", params: { threadId: session.id, status, agentFamily: "claude" } });
  }
  async observeSessionState(session, handle, message) {
    if (message?.type !== "system" || message.subtype !== "session_state_changed"
      || message.session_id !== (session.pendingReset?.newSessionId ?? session.sdkSessionId) || message.parent_tool_use_id
      || !["idle", "running", "requires_action"].includes(message.state)
      || this.processHandle(session) !== handle || handle.abort.signal.aborted) return;
    handle.sessionState = message.state;
    if (message.state === "idle") handle.followUp = null;
    await this.publishSessionStatus(session, handle);
  }
  async receive(frame) {
    if (!frame || typeof frame !== "object") throw new Error("Invalid runtime frame");
    if (!frame.method && frame.id != null) {
      const pending = this.pending.get(frame.id);
      if (!pending) return;
      this.pending.delete(frame.id); pending.cleanup();
      if (frame.error) pending.reject(new Error("The action was cancelled in Cupola.")); else pending.resolve(frame.result);
      return;
    }
    if (frame.id == null) return;
    try { await this.send({ id: frame.id, result: await this.request(frame.method, frame.params ?? {}) }); }
    catch (error) {
      const data = error instanceof InputRefusal && frame.method === error.method
        ? { [error.method === "turn/start" ? "turnStart" : "turnSteer"]: { accepted: false, action: error.action } } : undefined;
      await this.send({ id: frame.id, error: { code: -32000, message: error.message, ...(data ? { data } : {}) } });
    }
  }
  async request(method, params) {
    const release = params?.threadId ? this.sessions.retain(params.threadId) : () => {};
    try { return await this.requestRetained(method, params); }
    finally { release(); this.sessions.evict(); }
  }
  async requestRetained(method, params) {
    if (method === "initialize") return { userAgent: "Wonder Claude bridge", wonderBridge: { protocolVersion: BRIDGE_PROTOCOL, family: "claude" }, capabilities: { experimentalApi: true } };
    if (method === "account/read") {
      const catalog = await this.catalog(params.refresh);
      return { account: catalog.connected ? { type: catalog.accessKind === "API key" ? "apiKey" : "claudeSubscription", planType: catalog.subscription, accessKind: catalog.accessKind ?? "Claude subscription" } : null, requiresOpenaiAuth: false,
        agentFamily: "claude", connected: catalog.connected, runtime: this.updates.status() };
    }
    // SDK update status for Settings → Providers, and the owner's "check
    // now". A check stages a compatible patch; it switches over once no turn
    // uses the runtime, so running conversations are never interrupted.
    if (method === "runtime/update/read") return this.updates.status();
    if (method === "runtime/update/check") { await this.updates.refresh({ force: true }); return this.updates.status(); }
    if (method === "model/list") {
      const catalog = await this.catalog();
      return { data: offeredModels(catalog.models).map(m => ({ id: `claude:${m.id}`, model: `claude:${m.id}`,
        displayName: modelDisplayName(m), description: m.description, hidden: false,
        agentFamily: "claude", nativeModelIds: m.resolvedModel ? [m.resolvedModel] : [], isDefault: m.id === "haiku", supportedReasoningEfforts: (m.efforts ?? []).map(effort => ({ reasoningEffort: effort, description: `${effort[0].toUpperCase()}${effort.slice(1)}` })), defaultReasoningEffort: null,
        serviceTiers: m.fast ? [{ id: "default", name: "Standard", description: "Standard speed, standard usage" }, { id: "fast", name: "Fast", description: "Faster output, higher usage" }] : [],
        defaultServiceTier: m.fast ? "default" : null })), nextCursor: null };
    }
    if (method === "account/rateLimits/read") {
      const catalog = await this.catalog(true);
      if (!catalog.windows) throw new Error("Claude usage is temporarily unavailable. Refresh to try again.");
      return { agentFamily: "claude", windows: catalog.windows, additionalUsageAvailable: catalog.additionalUsageAvailable ?? null, planType: catalog.subscription ?? null, accountEmail: catalog.accountEmail ?? null };
    }
    if (["app/installed", "app/read", "mcpServerStatus/list"].includes(method)) {
      const session = params.threadId ? await this.sessions.load(params.threadId) : null;
      const running = session && this.active.get(session.id);
      const servers = running?.query ? await running.query.mcpServerStatus() : (await this.catalog(params.forceRefetch || params.forceRefresh)).servers;
      const visible = servers.filter(s => s.source !== "sdk" && s.name !== "wonder");
      if (method === "mcpServerStatus/list") return { data: visible, nextCursor: null };
      return { apps: visible.map(s => ({ id: `claude:${s.name}`, runtimeName: s.name,
        name: s.source === "claudeai" ? s.name.replace(/^claude\.ai(?:\s*[:/·-]\s*|\s+)/i, "") : s.name,
        // Only a claude.ai connector is fixed on claude.ai; "needs-auth" is the one state its sign-in fixes.
        installUrl: s.source === "claudeai" ? "https://claude.ai/settings/connectors" : null, enabled: s.status !== "disabled", callable: s.status === "connected",
        isEnabled: s.status !== "disabled", isAccessible: s.status !== "needs-auth" })), nextCursor: null };
    }
    if (method === "project/sessions/list") return this.projectSessions(params);
    if (method === "project/folders/list") return this.projectFolders(params);
    if (method === "project/configuration/read") {
      const project = await projectContext(params.wonderProject);
      const { fingerprint, empty, tooLarge, items, folders } = await folderConfiguration(project);
      return { fingerprint, empty, tooLarge, items, folders, trusted: folderTrusted({ fingerprint, empty }, project.trustedConfigurations) };
    }
    if (method === "project/session/attach") return this.attachProjectSession(params);
    if (method === "project/session/fork") return this.forkProjectSession(params);
    if (method === "thread/rewind") return this.rewindProjectSession(params);
    if (method === "thread/start") {
      selectedModel(params.model);
      params.wonderProject = await projectContext(params.wonderProject);
      const policy = new ToolPolicy({ ...params.wonderPolicy, cwd: params.cwd, projectRoots: [params.wonderProject.cwd, ...params.wonderProject.additionalDirectories], tools: advertisedTools(params).map(t => t.name) });
      if (!await policy.permits(params.cwd)) throw new Error("Claude's workspace is outside the allowed scope.");
      const session = await this.sessions.create(params);
      await this.send({ method: "thread/started", params: { thread: this.describe(session), agentFamily: "claude" } });
      return { thread: this.describe(session), model: params.model };
    }
    // Sessions whose Claude process holds work that ending this runtime would
    // lose: a reply running, or background agents, commands or monitors kept
    // after a reply. A warm process kept only for the next reply holds none
    // (Reconnect and updates may end it).
    // `threadIds` limits the answer to those threads, so a list never waits
    // on reading another chat's transcript.
    if (method === "thread/loaded/list") {
      if (params.threadIds != null && (!Array.isArray(params.threadIds) || params.threadIds.length > 200
        || params.threadIds.some(id => typeof id !== "string"))) throw new Error("Choose fewer Claude sessions to read.");
      const only = params.threadIds == null ? null : new Set(params.threadIds);
      const kept = [];
      for (const [id, handle] of this.lingering) {
        if (only && !only.has(id)) continue;
        if (this.processBusy(handle) || await this.lingerHasTasks(handle.session, handle)) kept.push(id);
      }
      return { data: [...new Set([...[...this.active.keys()].filter(id => !only || only.has(id)), ...kept])], nextCursor: null };
    }
    if (method === "thread/list") {
      // The sidebar asks once for its visible native sessions. Filter before
      // paging so retained sessions outside that page cannot hide active work.
      if (params.sessionIds != null && (!Array.isArray(params.sessionIds) || params.sessionIds.length > 200
        || params.sessionIds.some(id => typeof id !== "string"))) throw new Error("Choose fewer Claude sessions to read.");
      if (params.parentThreadId != null && typeof params.parentThreadId !== "string") throw new Error("Choose a Claude conversation to list its agents.");
      // Claude conversations and their agents are never archived here, so an
      // archived listing is empty rather than a copy of the current one.
      if (params.archived === true) return { data: [], nextCursor: null };
      const ids = params.sessionIds == null ? null : new Set(params.sessionIds);
      const parentId = params.parentThreadId ?? null;
      const listed = [];
      const candidates = ids ? this.sessions.withNative(ids) : parentId ? this.sessions.childrenOf(parentId) : this.sessions.all();
      for await (const session of candidates) {
        if ((params.sourceKinds && !session.parent) || (ids && (session.parent || !ids.has(session.sdkSessionId)))
          || (parentId && session.parent?.threadId !== parentId)) continue;
        if (session.parent?.nativeSessionId && (!this.sessions.ids.has(session.parent.threadId)
          || (await this.sessions.load(session.parent.threadId)).sdkSessionId !== session.parent.nativeSessionId)) continue;
        listed.push(this.describe(session));
      }
      return page(listed, params);
    }
    const session = await this.sessions.load(params.threadId);
    if (session.pendingReset && !this.active.has(session.id)) this.finishNativeReset(session).catch(() => {});
    if (method === "thread/commands/list") {
      return this.processHandle(session)?.commands?.read() ?? unavailableCommands(session.sdkSessionId);
    }
    if (method === "thread/nativeSettings") {
      const project = session.options.wonderProject;
      return { settings: session.sdkStarted ? await this.sessionFiles.latestSettings(session.sdkSessionId, project.cwd) : null };
    }
    if (["thread/turns/list", "thread/items/list"].includes(method)) {
      // The native transcript includes turns written by Claude Code on the Mac.
      const { turns, fingerprints } = await this.projectHistory(session);
      const items = (selected, turnId, asc) => [selected.flatMap(t => t.items.map(item => ({ turnId: t.id, item }))),
        JSON.stringify(["items", turnId || null, turnId ? null : asc])];
      // A turn can grow after it ends (Claude's own reply when a background task
      // finishes), so an unloaded turn still says how many items it has. The
      // task list and receipts can change an item without a transcript write
      // or a turn change: on request, `historyRevision` is the full item
      // list's own fingerprint (the one the host's item pages reuse), so the
      // host can tell an unchanged history without reading it.
      if (method === "thread/turns/list") {
        const listed = await page([...turns].reverse().map(t => params.itemsView === "notLoaded" ? { ...t, items: [], itemCount: t.items.length } : t), params,
          fingerprints, params.itemsView === "notLoaded" ? "turns:unloaded" : "turns:loaded");
        if (params.includeHistoryRevision !== true) return listed;
        const [all, view] = items(turns, null, true);
        return { ...listed, historyRevision: await fingerprintOf(all, fingerprints, view) };
      }
      const selected = params.turnId ? turns.filter(t => t.id === params.turnId) : params.sortDirection === "asc" ? turns : [...turns].reverse();
      const [data, view] = items(selected, params.turnId, params.sortDirection === "asc");
      return page(data, params, fingerprints, view);
    }
    if (method === "thread/backgroundTasks/list") {
      // Tasks run elsewhere only in a process Claude on the Mac has open for
      // this chat while Cupola runs none; without one, no transcript read.
      if (params.runningElsewhereOnly === true && await this.runsNothingElsewhere(session)) return { data: [] };
      // Lists cache both ownership cases. A closed, unowned chat needs no
      // transcript read, just as the elsewhere-only status request does.
      if (params.runningOnly === true && !this.active.has(session.id) && !this.lingering.has(session.id)
        && await this.runsNothingElsewhere(session)) return { data: [] };
      const tasks = (await this.projectTasks(session, null, { requireComplete: params.runningOnly === true }))
        .filter(task => params.runningOnly !== true || task.status === "running");
      return { data: tasks.map(({ request, result, summary, outputFile, taskId, ...row }) => row) };
    }
    if (method === "thread/backgroundTask/read") return this.backgroundTask(session, params.taskId);
    if (method === "thread/backgroundTask/stop") return this.stopBackgroundTask(session, params.taskId);
    if (method === "thread/read") {
      await this.closeStaleTurns(session);
      const contextUsage = params.includeContextUsage === true ? await this.contextUsage(session) : undefined;
      return { thread: this.describe(session, params.includeTurns === true), ...(contextUsage !== undefined ? { contextUsage } : {}) };
    }
    if (["thread/resume", "thread/settings/update"].includes(method)) {
      if (session.pendingReset) throw resetRefusal(session, method);
      if (this.sessionBusy(session)) {
        if (method === "thread/resume") return { thread: this.describe(session, params.excludeTurns !== true), model: session.options.model };
        throw new Error("Wait for Claude to finish before changing this conversation.");
      }
      if (params.model) selectedModel(params.model);
      const next = { ...session.options, ...params };
      new ToolPolicy({ ...next.wonderPolicy, cwd: next.cwd });
      session.options = next; await this.sessions.save(session);
      return { thread: this.describe(session, true), model: next.model };
    }
    if (method === "turn/start") {
      const replay = Boolean(params.clientUserMessageId && (session.receipts?.[params.clientUserMessageId]
        || session.turns.some(t => t.clientUserMessageId === params.clientUserMessageId)));
      // An already accepted request keeps its receipt even if the native
      // process became busy or unreadable since that request completed.
      if (replay) return { turn: (await this.sessions.accept(session, params)).turn };
      if (session.pendingReset) throw resetRefusal(session);
      if (this.rewinding.has(session.id)) throw new InputRefusal("Cupola is rewinding this conversation. Your message waits until it finishes.", { action: "wait" });
      if (this.processBusy(this.processHandle(session))) throw new InputRefusal(RUNNING_REFUSAL, { action: "wait" });
      // Two writers on one Claude Code session would fork its transcript.
      // Claude on the Mac keeps the conversation in its own process; Wonder's
      // host ends the desktop app's process before sending, so one still open
      // here started again since, or is a terminal Wonder cannot close.
      if (session.sdkStarted && !this.active.has(session.id)) {
        const mac = await claudeSessionState(session.sdkSessionId, this.claudeSessionsDir);
        if (mac.busy && mainTurnRunning((await this.projectMessages(session)).messages))
          throw new InputRefusal("Claude is working on this conversation on your computer. Send your message when it finishes.", { action: "wait" });
        if (mac.open == null) throw new InputRefusal("Claude's session state on your computer couldn't be read. Try again in a moment.", { action: "wait" });
        if (mac.open)
          throw new InputRefusal("This conversation is open in Claude on your computer. Send your message again in a moment.", { action: "wait" });
      }
      if (params.wonderProject) params.wonderProject = await projectContext(params.wonderProject);
      const options = { ...session.options, ...params };
      if (options.wonderProject.cwd !== session.options.wonderProject.cwd)
        throw new Error("A project conversation keeps its working folder.");
      selectedModel(options.model);
      const policy = new ToolPolicy({ ...options.wonderPolicy, cwd: options.wonderProject.cwd,
        structuredOutput: Boolean(options.outputSchema),
        projectRoots: [options.wonderProject.cwd, ...options.wonderProject.additionalDirectories],
        tools: advertisedTools(options).map(t => t.name), skills: wonderSkills(options)?.skills ?? [] });
      // Read only loads no configuration from disk, so it has none to trust.
      if (policy.settingSources.length) {
        const configuration = await folderConfiguration(options.wonderProject);
        options.folderTrust = { trusted: folderTrusted(configuration, options.wonderProject.trustedConfigurations), fingerprint: configuration.fingerprint,
          servers: configuration.servers };
      }
      const content = await sdkInput(params.input, policy);
      const context = wonderContext(params);
      // The process kept from the last reply serves this one when its shape
      // matches and nobody else wrote the transcript since (a process keeps
      // the conversation in memory, so continuing a stale one would fork it).
      // A different shape needs a new process, which would end background
      // work still running in the old one: refuse that instead.
      let reuse = this.lingering.get(session.id);
      const shape = processShape(options, policy);
      if (reuse && !this.active.has(session.id) && !replay) {
        if (reuse.shape !== shape && await this.lingerHasTasks(session, reuse)) throw new InputRefusal(BACKGROUND_REFUSAL);
        if (reuse.shape !== shape || await this.transcriptMoved(session, reuse)) { await this.endLinger(session.id, true, true); reuse = null; }
      }
      await this.closeStaleTurns(session);
      if (!replay && this.processBusy(this.processHandle(session))) throw new InputRefusal(RUNNING_REFUSAL, { action: "wait" });
      // The journal records the new turn as running before its run is
      // registered below. A read in between (the phone polls while it shows the
      // Mac's reply) must not close it as stale: see closeStaleTurns.
      const admission = Promise.withResolvers();
      this.admitting.set(session.id, admission.promise);
      let turn;
      try {
        const accepted = await this.sessions.accept(session, params);
        turn = accepted.turn;
        if (!accepted.duplicate) {
          reuse = this.lingering.get(session.id) === reuse ? reuse : null;
          // File backups follow the process that runs the turn: a kept one keeps
          // what it started with, a new one the owner's current setting.
          if (reuse ? reuse.checkpointing === true : options.fileCheckpointing === true) {
            turn.checkpointed = true; await this.sessions.save(session);
          }
          if (reuse) { reuse.suggestions.begin(); this.unlinger(session.id, reuse); }
          const run = { reuse, turn, policy, context, abort: new AbortController(), query: null, children: new Map(), childTasks: new Map(), stopped: false, authorizedTools: new Map(), toolResults: new Map(),
            roster: reuse?.roster ?? { tasks: null, toolUses: new Map(), startedAt: Date.now() } };
          this.active.set(session.id, run);
          // Return the durable receipt before the SDK can produce an event.
          run.finished = new Promise((resolve, reject) => setImmediate(() => {
            this.execute(session, run, options, policy, content).then(resolve, reject);
          }));
          run.finished.catch(() => this.onFatal());
        }
      } finally { this.admitting.delete(session.id); admission.resolve(); }
      return { turn };
    }
    // Guide: a message sent while a Project reply runs goes into the same
    // Claude process with priority "now". Claude Code cuts the running reply
    // short (keeping what it wrote) and answers this message next, as its own
    // turn; that turn's ID is returned like Codex's steer receipt.
    if (method === "turn/steer") {
      const replay = Boolean(params.clientUserMessageId && (session.receipts?.[params.clientUserMessageId]
        || session.turns.some(t => t.clientUserMessageId === params.clientUserMessageId)));
      // A replay joins any pending journal write and never queues input again.
      if (replay) return { turnId: (await this.sessions.accept(session, params, { steering: true })).turn.id };
      const run = this.active.get(session.id);
      if ((run?.steeredBy || run?.admitting) && run.turn.id === params.expectedTurnId)
        throw new InputRefusal("Claude is already taking your last Guide message. Queue this one instead.", { method: "turn/steer", action: "wait" });
      if (!run || run.turn.id !== params.expectedTurnId || !run.query || !run.input || run.stopped || run.finalizing)
        throw new InputRefusal("This reply already finished. Send your message instead.", { method: "turn/steer", action: "wait" });
      // Admission is reserved before any await, and the reply's finalization
      // waits for it (see execute): the Guide turn is then either handed the
      // process or failed explicitly, never acknowledged with no executor.
      run.admitting = (async () => {
        const content = await sdkInput(params.input, run.policy);
        const { turn } = await this.sessions.accept(session, params, { steering: true, checkpointed: run.handle?.checkpointing === true });
        run.steeredBy = { turn, content };
        run.input.push({ type: "user", session_id: session.sdkSessionId, parent_tool_use_id: null, uuid: turn.id,
          priority: "now", message: { role: "user", content } });
        return turn;
      })();
      try { return { turnId: (await run.admitting).id }; }
      finally { run.admitting = null; }
    }
    if (method === "turn/interrupt") {
      const actionId = params.ownerActionId;
      if (actionId != null && (typeof actionId !== "string" || actionId.length < 1 || actionId.length > 128 || /[^A-Za-z0-9_-]/.test(actionId)))
        throw new InputRefusal("This Stop action is invalid. Try Stop again.");
      let run = this.active.get(session.id);
      await run?.admitting?.catch(() => {}); // A Guide being saved is part of the reply.
      run = this.active.get(session.id);
      // After Guide the phone's current turn is the Guide's, which Claude has
      // not taken while it finishes the step it is on: Stop names either.
      if (run && (run.turn.id === params.turnId || run.steeredBy?.turn.id === params.turnId)) {
        // IDs belong to owner taps, not RPC requests. Replays and automatic
        // update handoffs cannot become the second tap. Only the first ID is
        // needed: the next distinct one ends this run; nothing carries to the next.
        const escalate = actionId != null && run.ownerStopActionId != null && actionId !== run.ownerStopActionId;
        if (actionId != null) run.ownerStopActionId ??= actionId;
        const kill = () => {
          if (this.active.get(session.id) !== run || run.hardStop) return;
          run.hardStop = true; run.abort.abort(); run.query?.close();
        };
        if (escalate) { kill(); return {}; }
        if (run.stopRequested || run.hardStop) return {};
        run.stopRequested = true;
        run.stopped = true;
        // A soft interrupt keeps the process, and its driver would finish the
        // computer action it is on: stop the driver as well.
        // Project turns declare perTaskStopAffordance, so a soft interrupt ends
        // only the reply; running agents stay until the user stops each one.
        // While a Guide waits, Claude ignores it and answers the Guide anyway
        // (measured with SDK 0.3.283 on haiku, Oct 12), so Stop ends the process.
        // Acknowledge admission without waiting for the SDK or driver. Either
        // can stall; the daemon must be able to deliver the next owner action.
        run.stopWork = (async () => {
          try {
            await run.cuaDriver?.stop();
            if (this.active.get(session.id) !== run || run.hardStop) return;
            if (run.sparesTasks && !run.steeredBy && typeof run.query?.interrupt === "function") {
              run.interrupting = true;
              await run.query.interrupt();
              return;
            }
          } catch { run.interrupting = false; }
          kill();
        })();
      } else await this.closeStaleTurns(session);
      return {};
    }
    // No silent no-ops for features that the SDK cannot faithfully provide.
    throw new Error("This action is not available for Claude yet.");
  }
  // `inProcess`: the caller checked the read is small enough to parse here.
  // A reader process costs about 0.1 s to start, and metadata reads (a
  // session's summary, its agent file listing) never need one.
  async withSdk(action, { inProcess = false } = {}) {
    const lease = await this.updates.acquire();
    try {
      const { sdk, entry } = lease.runtime;
      if (!entry || inProcess) return await action(sdk);
      return await this.readers.usingSdk(entry, readers => action({ ...sdk,
        ...Object.fromEntries(Object.entries(readers).filter(([name]) => typeof sdk[name] === "function" && !METADATA_READS.has(name))) }));
    }
    finally { await lease.release(); }
  }
  // Native file readers start no process or model work.
  async projectSessions(params) {
    return this.sessionFiles.listSessions(params);
  }
  // Suggestions for explicit inclusion: recent interactive Claude Code folders.
  // SDK-originated sessions are not suggested as desktop sessions.
  async projectFolders(params) {
    const limit = Number.isSafeInteger(params.limit) ? Math.max(1, Math.min(params.limit, 50)) : 20;
    return this.withSdk(async sdk => {
      const folders = new Map();
      for (const info of await sdk.listSessions({ limit: 200, includeWorktrees: false, includeProgrammatic: false })) {
        if (!info.cwd || !isAbsolute(info.cwd)) continue;
        const updatedAt = Math.floor(info.lastModified / 1000);
        if ((folders.get(info.cwd) ?? -1) < updatedAt) folders.set(info.cwd, updatedAt);
      }
      return { data: [...folders].sort((a, b) => b[1] - a[1]).slice(0, limit).map(([cwd, updatedAt]) => ({ cwd, updatedAt })) };
    });
  }
  async attachProjectSession(params) {
    const key = params.sessionId;
    const previous = this.attaching.get(key) ?? Promise.resolve();
    const attaching = previous.catch(() => {}).then(() => this.attachProjectSessionOnce(params));
    const tail = attaching.then(() => {}, () => {}); this.attaching.set(key, tail);
    try { return await attaching; }
    finally { if (this.attaching.get(key) === tail) this.attaching.delete(key); }
  }
  async attachProjectSessionOnce(params) {
    if (typeof params.sessionId !== "string" || !/^[0-9a-f-]{36}$/.test(params.sessionId)) throw new Error("Invalid Claude session.");
    const project = await projectContext(params.wonderProject);
    selectedModel(params.model);
    new ToolPolicy({ ...params.wonderPolicy, cwd: project.cwd });
    const existingId = await this.sessions.findId(s => s.sdkSessionId === params.sessionId && !s.parent, [params.sessionId]);
    const existing = existingId ? await this.sessions.load(existingId) : null;
    if (existing) {
      if (existing.options.wonderProject?.cwd !== project.cwd) throw new Error("This Claude session belongs to another folder.");
      return { thread: this.describe(existing) };
    }
    const info = await this.withSdk(sdk => sdk.getSessionInfo(params.sessionId, { dir: project.cwd }));
    // Never create a replacement: a missing transcript is a recoverable error.
    if (!info || info.cwd !== project.cwd) throw new Error("This Claude session is no longer available on your computer.");
    const session = await this.sessions.create({ ...params, wonderProject: project, cwd: project.cwd }, null, { sdkSessionId: params.sessionId, sdkStarted: true });
    return { thread: this.describe(session) };
  }
  // Copies the transcript, through the end of `lastTurnId` when given, into a new
  // native session. The source is never touched; the caller attaches the copy.
  async forkProjectSession(params) {
    const session = await this.sessions.load(params.threadId);
    if (session.pendingReset && !this.active.has(session.id)) this.finishNativeReset(session).catch(() => {});
    if (session.pendingReset) throw resetRefusal(session, "thread/fork");
    const project = session.options.wonderProject;
    if (!project || !session.sdkStarted) throw new Error("Send a message first. There is nothing to fork yet.");
    if (this.sessionBusy(session)) throw new Error("Wait for Claude to finish before forking this conversation.");
    await this.endLinger(session.id, true, true);
    const macBusy = await claudeSessionBusy(session.sdkSessionId, this.claudeSessionsDir);
    if (macBusy == null) throw new Error("Claude's session state on your computer couldn't be read. Try again in a moment.");
    if (macBusy) throw new Error("Claude is working on this conversation on your computer. Fork it when it finishes.");
    // After Edit from here, the conversation ends where the rewind cut it.
    let upToMessageId = session.rewind?.at;
    if (params.lastTurnId != null) {
      if (typeof params.lastTurnId !== "string") throw new Error("Choose a reply to fork from.");
      upToMessageId = turnEndMessageId((await this.projectMessages(session)).messages, params.lastTurnId);
      if (!upToMessageId) throw new Error("That reply is no longer in this conversation.");
    }
    const title = typeof params.title === "string" && params.title.trim() ? params.title.trim().slice(0, 200) : undefined;
    const forked = await this.withSdk(sdk => sdk.forkSession(session.sdkSessionId,
      { dir: project.cwd, ...(upToMessageId ? { upToMessageId } : {}), ...(title ? { title } : {}) }));
    return { sessionId: forked.sessionId };
  }
  // Edit from here: the conversation continues from before the owner's
  // message that opened `turnId`. Claude Code keeps its transcript; the next
  // reply resumes at the entry before that message (`resumeSessionAt`), which
  // drops it and everything after from Claude's context, and until then
  // history ends there too. Rewinding the first message starts a new native
  // session instead (the old transcript stays on disk). With `restoreFiles`,
  // Claude first puts back the files edited from that message on, from the
  // backups it kept (`rewindFiles`); that needs every removed reply to have
  // run with backups on, and nothing changes when it cannot.
  async rewindProjectSession(params) {
    const session = await this.sessions.load(params.threadId);
    if (session.parent) throw new Error("Edit an agent's work in its parent conversation.");
    if (session.pendingReset && !this.active.has(session.id)) this.finishNativeReset(session).catch(() => {});
    if (session.pendingReset) throw resetRefusal(session, "thread/rewind");
    if (typeof params.turnId !== "string" || !params.turnId) throw new Error("Choose one of your messages.");
    if (!session.sdkStarted) throw new Error("Send a message first. There is nothing to edit yet.");
    if (this.sessionBusy(session) || this.rewinding.has(session.id)) throw new Error("Wait for Claude to finish before editing this conversation.");
    this.rewinding.add(session.id);
    try {
      // A kept process holds the whole conversation in memory.
      await this.endLinger(session.id, true, true);
      if (this.sessionBusy(session)) throw new Error("Wait for Claude to finish before editing this conversation.");
      const mac = await claudeSessionState(session.sdkSessionId, this.claudeSessionsDir);
      if (mac.open == null) throw new Error("Claude's session state on your computer couldn't be read. Try again in a moment.");
      if (mac.busy) throw new Error("Claude is working on this conversation on your computer. Edit it when it finishes.");
      // Claude on the Mac would continue from its own copy and bring the
      // removed messages back.
      if (mac.open) throw new Error("This conversation is open in Claude on your computer. Close it there, then edit it here.");
      const { messages, compacted } = await this.projectMessages(session);
      const point = rewindPoint(messages, params.turnId);
      if (!point) throw new Error("That message is no longer in this conversation.");
      if (point.index < compacted) throw new Error("Claude compacted this conversation after that message, so it can't go back that far. Edit a later message.");
      const removed = (await nativeTurnsAsync(messages.slice(point.index))).map(turn => turn.id);
      const journal = session.turns.findIndex(turn => turn.id === params.turnId);
      if (journal >= 0) for (const turn of session.turns.slice(journal)) if (!removed.includes(turn.id)) removed.push(turn.id);
      if (removed[0] !== params.turnId) throw new Error("That message is no longer in this conversation.");
      let files = null;
      if (params.restoreFiles === true) {
        if (!removed.every(id => session.turns.some(turn => turn.id === id && turn.checkpointed === true)))
          throw new Error("Claude didn't keep file backups for every reply from this message on, so Cupola can't restore files. Rewind the conversation only, or restore them in Claude on your computer.");
        files = await this.restoreFiles(session, params.turnId);
      }
      const previous = session.sdkSessionId;
      if (point.keep) session.rewind = { turnId: params.turnId, at: point.keep };
      else {
        for await (const child of this.sessions.childrenOf(session.id)) {
          if (child.parent?.threadId === session.id && !child.parent.nativeSessionId) {
            child.parent.nativeSessionId = previous;
            await this.sessions.save(child);
          }
        }
        session.sdkSessionId = randomUUID(); session.sdkStarted = false; delete session.rewind;
      }
      const gone = new Set(removed);
      session.turns = session.turns.filter(turn => !gone.has(turn.id));
      await this.sessions.save(session);
      this.transcripts.delete(previous); this.taskProjections.delete(session.id);
      return { removedTurnIds: removed, filesRestored: Boolean(files),
        ...(files ? { filesChanged: files.filesChanged?.length ?? 0, ...(files.skippedLinks ? { filesSkipped: files.skippedLinks } : {}) } : {}),
        ...(point.keep ? {} : { sessionId: session.sdkSessionId }) };
    } finally { this.rewinding.delete(session.id); }
  }
  // Restores files from Claude's own backups in a process that only does
  // that: no prompt is sent and no model runs. A dry run first confirms every
  // file can be restored.
  async restoreFiles(session, turnId) {
    const lease = await this.updates.acquire();
    const input = channel(), abort = new AbortController();
    let query;
    try {
      const { sdk } = lease.runtime;
      query = sdk.query({ prompt: (async function* () { for (let next = await input.next(); next; next = await input.next()) yield next; })(),
        options: { ...baseOptions(lease.runtime), cwd: session.options.wonderProject.cwd, resume: session.sdkSessionId,
          enableFileCheckpointing: true, persistSession: false, abortController: abort, tools: [],
          canUseTool: async () => ({ behavior: "deny", message: "Cupola is only restoring files." }) } });
      await query.initializationResult();
      const preview = await query.rewindFiles(turnId, { dryRun: true });
      if (!preview?.canRewind) throw new Error("Claude can't restore the files from this message. Nothing was changed.");
      const result = await query.rewindFiles(turnId);
      if (!result?.canRewind) throw new Error("Claude couldn't restore the files. Check them on your computer; the conversation was not changed.");
      return result;
    } finally { input.close(); query?.close(); abort.abort(); await lease.release(); }
  }
  projectMessages(session) { return retryingAppends(() => this.projectMessagesOnce(session)); }
  async projectMessagesOnce(session) {
    const key = session.sdkSessionId;
    let reading = this.messageReads.get(key);
    if (!reading) {
      reading = this.readProjectMessages(session); this.messageReads.set(key, reading);
    }
    try { return await reading; }
    finally { if (this.messageReads.get(key) === reading) this.messageReads.delete(key); }
  }
  async readProjectMessages(session) {
    if (!session.sdkStarted) return { messages: [], desktop: false, stamp: null };
    if (session.pendingReset) throw new Error("Claude is resetting this conversation. Try again in a moment.");
    const nativeId = session.sdkSessionId, epoch = this.historyEpoch;
    const cwd = session.options.wonderProject.cwd;
    // History pages call this repeatedly; reuse the read until the transcript changes.
    const cached = this.transcripts.get(nativeId);
    // An uncertain refresh must not leave an older projection reusable.
    this.transcripts.delete(nativeId);
    const stamp = await this.sessionFiles.stamp(nativeId, cwd);
    let messages, compacted = 0;
    if (stamp && cached?.stamp === stamp) ({ messages, compacted = 0 } = cached);
    else {
      // A transcript under 8 MiB parses here with event-loop pauses under 5 ms
      // (measured, Node 24); larger ones keep the leased reader process.
      const located = await this.sessionFiles.locate?.(nativeId, cwd);
      const size = located ? (await stat(located).catch(() => null))?.size : undefined;
      const recent = await this.withSdk(sdk => sdk.getSessionMessages(nativeId, { dir: cwd }),
        { inProcess: Number.isFinite(size) && size < this.inProcessReadBytes });
      // The SDK reader begins at the latest compaction; keep the turns before it.
      const { earlier, edits } = recent.length ? await this.sessionFiles.messageContext(nativeId, cwd, recent[0].uuid)
        : { earlier: [], edits: new Map() };
      const withEdits = recent.map(m => m.type === "user" && edits.has(m.uuid) ? { ...m, tool_use_result: edits.get(m.uuid) } : m);
      messages = earlier.length ? [...earlier, ...withEdits] : withEdits;
      compacted = earlier.length;
    }
    // A reply started on the Mac (Claude desktop or terminal) is running there
    // only while its main turn is unfinished; background agents, commands and
    // monitors it left running do not make the conversation busy.
    const mac = this.active.has(session.id) ? { busy: false, background: false }
      : await claudeSessionState(nativeId, this.claudeSessionsDir);
    // The SDK drops unfinished records and exposes no read revision. Check
    // framing and revision around the whole read, including native liveness.
    const currentStamp = await this.sessionFiles.stamp(nativeId, cwd);
    if (session.pendingReset || session.sdkSessionId !== nativeId || this.historyEpoch !== epoch) throw new Error(UPDATING);
    if (currentStamp !== stamp) throw changed(stamp, currentStamp);
    if (stamp) {
      const record = cached?.stamp === stamp ? cached : { stamp, messages, compacted };
      record.bytes ??= await retainedBytes(record);
      const afterCharge = await this.sessionFiles.stamp(nativeId, cwd);
      if (session.pendingReset || session.sdkSessionId !== nativeId || this.historyEpoch !== epoch) throw new Error(UPDATING);
      if (afterCharge !== stamp) throw changed(stamp, afterCharge);
      this.transcripts.set(nativeId, record, record.bytes);
    }
    while (this.transcripts.size > 4) this.transcripts.delete(this.transcripts.keys().next().value);
    // Edit from here: until the next reply writes the new branch, the
    // conversation ends before the rewound message. Once Claude's transcript no
    // longer contains it, the rewind is complete.
    if (session.rewind) {
      const point = rewindPoint(messages, session.rewind.turnId);
      if (point) messages = messages.slice(0, point.index);
      else { delete session.rewind; await this.sessions.save(session); }
    }
    // Claude Code 2.1.295 reports "idle" while background commands run, so any
    // open process runs the tasks its transcript launched and has not reported.
    if (this.historyEpoch !== epoch) throw new Error("Claude is updating this conversation. Try again in a moment.");
    return { messages, compacted, stamp, desktop: mac.busy == null ? null : mac.busy && mainTurnRunning(messages), desktopTasks: mac.background || Boolean(mac.open),
      desktopStartedAt: mac.startedAt ?? null };
  }
  // Claude Code ends a background command's output file with "[exited with
  // code N]" when it finishes; that holds even when no notice reached the
  // transcript. Only this session's own regular output file is read.
  async commandExit(session, taskId) {
    if (typeof taskId !== "string" || !/^[A-Za-z0-9_-]{1,64}$/.test(taskId)) return null;
    const path = join(this.claudeTasksRoot, projectFolderName(session.options.wonderProject.cwd), session.sdkSessionId, "tasks", `${taskId}.output`);
    try {
      const info = await lstat(path);
      if (!info.isFile()) return null;
      const handle = await open(path, "r");
      try {
        const length = Math.min(info.size, 256);
        const buffer = Buffer.alloc(length);
        await handle.read(buffer, 0, length, info.size - length);
        const tail = buffer.toString("utf8");
        // Claude Code ends the output with the exit code, or "[killed]" when the task was stopped.
        if (/\[killed\]\s*$/.test(tail)) return "killed";
        const code = /\[exited with code (\d+)\]\s*$/.exec(tail)?.[1];
        return code == null ? null : Number(code);
      } finally { await handle.close(); }
    } catch { return null; }
  }
  // Validate transcript framing, identity and runtime epoch around task-only
  // metadata just as history reads do. Liveness, helper files and output exit
  // markers are rechecked separately; a transcript stamp alone is not status.
  async projectTaskMessages(session) {
    const key = JSON.stringify([this.historyEpoch, session.sdkSessionId, session.rewind ?? null]);
    let reading = this.taskMessageReads.get(key);
    if (!reading) {
      if (this.taskMessageReads.size >= 8) throw new Error("Claude is still reading task status. Try again in a moment.");
      reading = retryingAppends(() => this.readProjectTaskMessages(session));
      this.taskMessageReads.set(key, reading);
    }
    try { return await reading; }
    finally { if (this.taskMessageReads.get(key) === reading) this.taskMessageReads.delete(key); }
  }
  async readProjectTaskMessages(session) {
    if (!session.sdkStarted) return { messages: [], stamp: null, desktopTasks: false };
    if (session.pendingReset) throw new Error(UPDATING);
    const nativeId = session.sdkSessionId, epoch = this.historyEpoch, rewind = session.rewind;
    const cwd = session.options.wonderProject.cwd;
    const stamp = await this.sessionFiles.stamp(nativeId, cwd);
    let messages = await this.sessionFiles.taskMessages(nativeId, cwd);
    const mac = this.active.has(session.id) ? { busy: false, background: false }
      : await claudeSessionState(nativeId, this.claudeSessionsDir);
    const currentStamp = await this.sessionFiles.stamp(nativeId, cwd);
    if (session.pendingReset || session.sdkSessionId !== nativeId || this.historyEpoch !== epoch || session.rewind !== rewind) throw new Error(UPDATING);
    if (stamp !== currentStamp) throw changed(stamp, currentStamp);
    if (mac.busy == null) throw new Error("Claude's session state on your computer couldn't be read. Try again in a moment.");
    if (rewind) {
      // Rewind was validated against full history when saved. Metadata keeps
      // the prompt's UUID even when its image/text payload is omitted.
      const index = messages.findIndex(message => message.uuid === rewind.turnId);
      if (index >= 0) messages = messages.slice(0, index);
    }
    return { messages, stamp, desktopTasks: mac.background || Boolean(mac.open), desktopStartedAt: mac.startedAt ?? null };
  }
  async projectTasks(session, observation = null, { requireComplete = false } = {}) {
    const epoch = this.historyEpoch, nativeId = session.sdkSessionId, rewind = session.rewind;
    const validate = () => {
      if (session.pendingReset || session.sdkSessionId !== nativeId || this.historyEpoch !== epoch || session.rewind !== rewind) throw new Error(UPDATING);
    };
    const { messages, stamp, desktopTasks, desktopStartedAt } = observation ?? await this.projectTaskMessages(session);
    const files = session.sdkStarted ? await this.sessionFiles.agentTasks(session.sdkSessionId, session.options.wonderProject.cwd, { requireComplete }) : [];
    validate();
    const run = this.active.get(session.id) ?? this.lingering.get(session.id);
    // Rebuilt only when the transcript, helper files or liveness changed: the
    // open conversation asks every few seconds while nothing does.
    const open = desktopTasks || Boolean(run);
    // Only the process holding the session now runs anything: Wonder's own,
    // or Claude on the Mac's. Tasks from processes that exited ended with them.
    const since = run ? run.roster?.startedAt ?? null : desktopStartedAt;
    let projection = this.taskProjections.get(session.id);
    const revision = stamp ? JSON.stringify([epoch, stamp, rewind ?? null]) : null;
    if (!revision || projection?.revision !== revision || projection.files !== files || projection.open !== open || projection.since !== since) {
      projection = { revision, files, open, since, tasks: await backgroundTasksAsync(messages, open, files, { since }) };
      const bytes = await retainedBytes(projection);
      validate();
      this.taskProjections.delete(session.id);
      this.taskProjections.set(session.id, projection, bytes);
      while (this.taskProjections.size > 8) this.taskProjections.delete(this.taskProjections.keys().next().value);
    }
    let tasks = withLiveRoster(projection.tasks, run?.roster);
    tasks = await Promise.all(tasks.map(async task => {
      if (task.status !== "running" || task.kind !== "command" || !task.taskId) return task;
      const code = await this.commandExit(session, task.taskId);
      return code == null ? task : { ...task, status: code === "killed" ? "interrupted" : code === 0 ? "completed" : "failed" };
    }));
    // Helper reads and output checks also await: never publish an answer from
    // a previous native session, runtime or rewind after those have changed.
    validate();
    if (stamp) {
      const currentStamp = await this.sessionFiles.stamp(nativeId, session.options.wonderProject.cwd);
      if (currentStamp !== stamp) throw changed(stamp, currentStamp);
      validate();
    }
    // Wonder can stop only tasks inside a session it is driving right now; a
    // session running in Claude on the Mac keeps its own stop controls.
    return tasks.map(task => {
      if (task.status === "running" && run?.stoppedTasks?.has(task.id)) task = { ...task, status: "interrupted" };
      const running = task.status === "running";
      return { ...task, progressSummary: running ? task.progressSummary ?? null : null,
        canStop: running && Boolean(run?.query) && Boolean(task.taskId),
        runningElsewhere: running && !run && desktopTasks };
    });
  }
  // Ask Claude Code to stop one running agent task or background command. The
  // SDK's stop_task control is the same one the Claude desktop app sends. The
  // outcome is data, not an exception, so the host can explain each case.
  async stopBackgroundTask(session, taskId) {
    if (typeof taskId !== "string" || !taskId || taskId.length > 256) throw new Error("Choose an agent task.");
    const task = (await this.projectTasks(session)).find(t => t.id === taskId);
    if (!task) return { outcome: "notFound" };
    const { request, result, outputFile, taskId: sdkTaskId, ...summary } = task;
    if (task.runningElsewhere) return { outcome: "runningElsewhere", task: summary };
    if (task.status !== "running") return { outcome: "notRunning", task: summary };
    const run = this.active.get(session.id) ?? this.lingering.get(session.id);
    if (!task.canStop || typeof run?.query?.stopTask !== "function") return { outcome: "unsupported", task: summary };
    await run.query.stopTask(sdkTaskId);
    (run.stoppedTasks ??= new Set()).add(task.id);
    return { outcome: "stopped", task: { ...summary, status: "interrupted", canStop: false, progressSummary: null } };
  }
  projectHistory(session) { return retryingAppends(() => this.projectHistoryOnce(session)); }
  async projectHistoryOnce(session) {
    if (!session.sdkStarted) return { turns: session.turns };
    const nativeId = session.sdkSessionId, epoch = this.historyEpoch;
    const observation = await this.projectMessages(session);
    const validate = async () => {
      const stamp = await this.sessionFiles.stamp(nativeId, session.options.wonderProject.cwd);
      if (session.pendingReset || session.sdkSessionId !== nativeId || this.historyEpoch !== epoch) throw new Error(UPDATING);
      if (stamp !== observation.stamp) throw changed(observation.stamp, stamp);
    };
    const { messages, desktop } = observation;
    if (desktop == null) throw new Error("Claude's session state on your computer couldn't be read. Try again in a moment.");
    const active = this.active.has(session.id) || desktop;
    // Reuse history only inside the existing four-transcript cache. Its file
    // stamp owns invalidation; a missing stamp never permits reuse. Receipts
    // can also change a prompt, outcome or error without a transcript write.
    const cached = this.transcripts.get(session.sdkSessionId);
    const receipts = await historyFingerprint(session.turns.map(t => ({ id: t.id, status: t.status,
      error: t.error ?? null, user: t.items?.find(i => i.type === "userMessage") })));
    const history = cached?.messages === messages ? cached.history : null;
    // A background command or monitor the transcript never saw end settles by
    // the session's task list (live roster, output file), not by a guess.
    let liveTasks = null;
    try {
      liveTasks = (await this.projectTasks(session, observation)).filter(task => task.kind !== "agent").map(({ id, status, summary }) => ({ id, status, summary }));
    } catch {}
    const taskState = JSON.stringify(liveTasks?.map(task => [task.id, task.status]) ?? null);
    if (history?.receipts === receipts && history.active === active && history.desktop === desktop && history.taskState === taskState) {
      await validate(); return history;
    }
    const turns = await nativeTurnsAsync(messages, session.turns, active, liveTasks);
    if (desktop && turns.length) turns.at(-1).runningElsewhere = true;
    const projected = { turns, receipts, active, desktop, taskState, fingerprints: new Map() };
    if (cached?.messages === messages) {
      cached.history = projected; cached.bytes = await retainedBytes(cached);
    }
    await validate();
    if (cached && this.transcripts.get(nativeId) === cached) this.transcripts.set(nativeId, cached, cached.bytes);
    return projected;
  }
  // Read-only detail for one agent task or background command.
  async backgroundTask(session, taskId) {
    if (typeof taskId !== "string" || !taskId || taskId.length > 256) throw new Error("Choose an agent task.");
    const task = (await this.projectTasks(session)).find(t => t.id === taskId);
    if (!task) throw new Error("This agent task is no longer in the conversation.");
    const { request, result, outputFile, ...summary } = task;
    const sections = [];
    if (task.request) sections.push(task.kind === "agent" ? `Task\n\n${task.request}` : `Command\n\n\`\`\`\n${task.request}\n\`\`\``);
    const work = task.kind === "agent" ? await this.agentWork(session, task) : [];
    if (task.summary && !work.length) sections.push(task.summary);
    if (task.result && !/^Async agent launched/i.test(task.result)) sections.push(task.result);
    const output = ["command", "monitor"].includes(task.kind) ? await this.taskOutput(session, task) : null;
    if (output) sections.push(`Latest output\n\n\`\`\`\n${output}\n\`\`\``);
    const items = sections.map((text, index) => ({ type: "agentMessage", id: `${task.id}:${index}`, text, status: "completed" }));
    return { task: summary, items: [...items.slice(0, 1), ...work, ...items.slice(1)] };
  }
  // The agent's own transcript, projected like the parent's history.
  async agentWork(session, task) {
    if (!task.taskId || !/^[A-Za-z0-9_-]{1,64}$/.test(task.taskId)) return [];
    try {
      const messages = await this.withSdk(sdk => sdk.getSubagentMessages?.(session.sdkSessionId, task.taskId,
        { dir: session.options.wonderProject.cwd }) ?? []);
      // Every message names its parent task; inside its own transcript it is top level.
      const own = messages.map(message => ({ ...message, parent_tool_use_id: null }));
      const turns = await nativeTurnsAsync(own, [], task.status === "running");
      // The host's agent task view shows work, not thinking; keep its 200 slots for that.
      return turns.flatMap(turn => turn.items.filter(item => !["userMessage", "reasoning"].includes(item.type))).slice(-200);
    } catch { return []; }
  }
  // Only Claude Code's own task output file for this exact session and task.
  async taskOutput(session, task) {
    const path = task.outputFile;
    if (typeof path !== "string" || !task.taskId || !/^[A-Za-z0-9_-]{1,64}$/.test(task.taskId)) return null;
    if (!/^\/(private\/)?tmp\/claude-[0-9]+\//.test(path) || basename(path) !== `${task.taskId}.output`
      || !path.includes(`/${session.sdkSessionId}/tasks/`)) return null;
    try {
      const info = await lstat(path);
      if (!info.isFile() || (await realpath(path)) !== (path.startsWith("/tmp/") ? `/private${path}` : path)) return null;
      const handle = await open(path, "r");
      try {
        const length = Math.min(info.size, 32 * 1024);
        const buffer = Buffer.alloc(length);
        await handle.read(buffer, 0, length, info.size - length);
        const text = buffer.toString("utf8").replace(/^[^\n]*\n/, info.size > length ? "" : "$&").replace(/\u001b\[[0-9;]*[A-Za-z]/g, "");
        return text.trim() ? text.trimEnd() : null;
      } finally { await handle.close(); }
    } catch { return null; }
  }
  serverCall(method, params, signal) {
    if (signal?.aborted) return Promise.reject(new Error("Claude action cancelled."));
    const id = `claude-request-${randomUUID()}`;
    return new Promise((resolve, reject) => {
      const abort = () => { this.pending.delete(id); cleanup(); reject(new Error("Claude action cancelled.")); };
      const cleanup = () => signal?.removeEventListener("abort", abort);
      signal?.addEventListener("abort", abort, { once: true });
      this.pending.set(id, { resolve, reject, cleanup, method, params });
      Promise.resolve(this.send({ id, method, params })).catch(error => { this.pending.delete(id); cleanup(); reject(error); });
    });
  }
  async permission(session, run, policy, name, input, context) {
    const base = { threadId: session.id, turnId: run.turn.id, itemId: context.toolUseID, agentFamily: "claude" };
    const deny = message => ({ behavior: "deny", message });
    if (name.startsWith("mcp__wonder__") && !policy.tools.has(name.slice(13))) return deny("This Cupola tool is unavailable for this Project.");
    const decision = await policy.decision(name, input);
    if (decision === "deny") return deny(policy.denial(name));
    if (name.startsWith("mcp__cua_driver__")) {
      const refusal = await this.computerControl(base, context.signal ?? run.abort.signal);
      if (refusal) return deny(refusal);
    }
    if (name === "AskUserQuestion") {
      const request = questionRequest(input, context.requestId ?? context.toolUseID);
      const answer = await this.serverCall("item/tool/requestUserInput", { ...base, ...request }, context.signal ?? run.abort.signal);
      return { behavior: "allow", updatedInput: questionAnswer(input, request, answer) };
    }
    if (name.startsWith("mcp__wonder__") && policy.tools.has(name.slice(13))) {
      if (context.mcpServer?.source !== "sdk" || context.mcpServer?.name !== "wonder") return deny("The tool's Cupola origin could not be verified.");
      const key = toolCallKey(name.slice(13), input);
      const calls = run.authorizedTools.get(key) ?? [];
      if (!calls.includes(context.toolUseID)) calls.push(context.toolUseID);
      run.authorizedTools.set(key, calls);
      return { behavior: "allow", updatedInput: input }; // The daemon owns these tools' approval and scope checks.
    }
    const allow = (updatedPermissions) => ({ behavior: "allow", updatedInput: input, ...(updatedPermissions ? { updatedPermissions } : {}) });
    if (decision === "allow" || (name.startsWith("mcp__") && policy.bypassesApproval)) return allow();
    if (name === "SandboxNetworkAccess") return this.networkApproval(base, run, input, context);
    const change = await proposedChange(name, input, policy.cwd);
    if (change) {
      const response = await this.serverCall("item/fileChange/requestApproval", { ...base, changes: [change],
        reason: context.title ?? "Allow Claude to change this file?", availableDecisions: ["accept", "decline", "cancel"] },
      context.signal ?? run.abort.signal);
      return response?.decision === "accept" ? allow() : deny("The owner declined this change.");
    }
    const command = isCommandTool(name);
    const outside = command && input.dangerouslyDisableSandbox === true;
    const response = await this.serverCall("item/commandExecution/requestApproval", { ...base,
      kind: command ? "command" : "tool",
      ...(command ? { supportsDenyMessage: true } : {}),
      command: command ? String(input.command ?? "") : `${name}: ${JSON.stringify(input)}`,
      cwd: policy.cwd, reason: outside ? "Run this command outside the sandbox, with full access to your computer and the network?"
        : context.title ?? "Allow Claude to perform this action?",
      availableDecisions: ["accept", "decline", "cancel"] }, context.signal ?? run.abort.signal);
    // The owner's instruction is Claude's native denial feedback, never a new turn.
    if (command && response?.decision === "decline" && typeof response.message === "string"
      && response.message.trim() && Buffer.byteLength(response.message, "utf8") <= 2000) return deny(response.message);
    // Approval can wait while the owner takes control. Recheck at the last
    // permission boundary before the SDK is allowed to execute the action.
    if (response?.decision === "accept" && name.startsWith("mcp__cua_driver__")) {
      const refusal = await this.computerControl(base, context.signal ?? run.abort.signal);
      if (refusal) return deny(refusal);
    }
    return response?.decision === "accept" ? allow() : deny(outside
      ? "The owner declined running this command outside the sandbox. Continue inside the sandbox or explain what the owner can change."
      : "The owner declined this action.");
  }
  // Asked before each computer-use action. Only the daemon's explicit,
  // successful "no" lets the action run; an error, a failed check or an
  // unreadable answer refuses it rather than risk acting while the owner is
  // using the computer.
  async computerControl(base, signal) {
    try {
      const result = await this.serverCall("item/tool/call", { threadId: base.threadId, turnId: base.turnId,
        callId: base.itemId, tool: CONTROL_CHECK, arguments: {}, agentFamily: base.agentFamily }, signal);
      if (result?.success !== true) return CONTROL_UNKNOWN;
      const held = JSON.parse(result.contentItems?.[0]?.text ?? "").ownerHasControl;
      return held === false ? null : held === true ? OWNER_HAS_CONTROL : CONTROL_UNKNOWN;
    } catch { return CONTROL_UNKNOWN; }
  }
  // A sandboxed command reached a host outside the allowlist. Saving the rule
  // writes Claude Code's own project setting, as the CLI's "always allow" does.
  async networkApproval(base, run, input, context) {
    const host = typeof input.host === "string" ? input.host.toLowerCase() : "";
    if (!/^[a-z0-9.-]{1,253}$/.test(host) || !host.includes(".")) return { behavior: "deny", message: "The sandbox blocked an invalid host." };
    const rule = (destination) => [{ type: "addRules", rules: [{ toolName: "WebFetch", ruleContent: `domain:${host}` }], behavior: "allow", destination }];
    const amendment = { applyNetworkPolicyAmendment: { network_policy_amendment: { host, action: "allow" } } };
    const response = await this.serverCall("item/commandExecution/requestApproval", { ...base,
      command: `Connect to ${host}`, cwd: null, reason: `The sandbox blocked ${host}. Allow this conversation's commands to connect to it?`,
      networkApprovalContext: { host, protocol: "https" },
      availableDecisions: ["accept", "acceptForSession", amendment, "decline"] }, context.signal ?? run.abort.signal);
    const decision = response?.decision;
    if (decision === "accept") return { behavior: "allow", updatedInput: input };
    if (decision === "acceptForSession") return { behavior: "allow", updatedInput: input, updatedPermissions: rule("session") };
    if (decision?.applyNetworkPolicyAmendment?.network_policy_amendment?.host === host)
      return { behavior: "allow", updatedInput: input, updatedPermissions: rule("localSettings") };
    return { behavior: "deny", message: `The owner did not allow ${host}. Continue without it, or ask the owner to allow it or choose Full access.` };
  }
  async applyModelSettings(handle, options) {
    const deadline = Date.now() + 2000;
    const wait = async change => {
      let timer;
      try {
        await Promise.race([change, new Promise((_, reject) => {
          timer = setTimeout(() => reject(new Error("Claude did not confirm the model change. Your message was not sent. Try again.")), Math.max(0, deadline - Date.now()));
        })]);
      } finally { clearTimeout(timer); }
    };
    // A timed-out native request can still settle. Keep it on this process so
    // a later message cannot overtake it or run with a late, older selection.
    if (handle.settingsChange) await wait(handle.settingsChange);
    if (!handle.settingsUnconfirmed && handle.settings.model === options.model
      && handle.settings.effort === (options.effort ?? null) && handle.settings.speed === choiceKey(options)) return;
    const change = Promise.resolve().then(() => handle.query.applyFlagSettings(modelSettings(options))).then(() => {
      handle.model = selectedModel(options.model);
      handle.settings = { model: options.model, effort: options.effort ?? null, speed: choiceKey(options) };
      handle.settingsUnconfirmed = false;
    }, error => { handle.settingsUnconfirmed = true; throw error; });
    handle.settingsChange = change;
    const clear = () => { if (handle.settingsChange === change) handle.settingsChange = null; };
    change.then(clear, clear);
    await wait(change);
  }
  async execute(session, run, options, policy, content) {
    let lease, query, iterator, handle, first = null;
    let suggestionSuppressed = policy.planMode;
    const reuse = run.reuse;
    const input = reuse?.input ?? channel(), done = { resolve: () => input.close() }, submit = Promise.withResolvers();
    const live = reuse?.live ?? { run, policy };
    run.input = input;
    const userMessage = () => {
      (live.context ??= []).push(...run.context ?? []);
      return { type: "user", session_id: session.sdkSessionId, parent_tool_use_id: null,
        uuid: run.turn.id, message: { role: "user", content } };
    };
    const output = [];
    const projection = new TurnProjection({ threadId: session.id, turnId: run.turn.id,
      promptUuid: run.turn.id, tasks: await this.taskList(session),
      emit: event => output.push(event), onSession: id => { if (!session.pendingReset) { session.sdkSessionId = id; session.sdkStarted = true; } },
      onChild: message => { run.childMessages.push(message); } });
    run.childMessages = reuse?.childMessages ?? [];
    const flush = async (terminal = false) => {
      // Child cleanup may append its last parent activity after the parent's
      // result. Publish all item updates before the terminal turn snapshot.
      if (terminal) output.sort((a, b) => Number(a.method === "turn/completed") - Number(b.method === "turn/completed"));
      while (output.length && (terminal || output[0].method !== "turn/completed")) await this.send(output.shift());
    };
    // Last sender wins: a reply Claude on the Mac starts in this chat after
    // this one began ends this one, so one process writes the transcript.
    const startedAt = Date.now();
    const watch = setInterval(() => this.yieldToMac(session, run, startedAt), this.yieldCheckMs);
    if (watch) watch.unref?.();
    try {
      projection.start(); await flush();
      if (reuse) {
        // Same process: the new reply joins the open prompt stream.
        // A skill loaded earlier in this process keeps its tool grants.
        policy.skillLoaded ||= live.policy.skillLoaded;
        lease = reuse.lease; query = reuse.query; handle = reuse; live.run = run; live.policy = policy;
        run.query = query; run.handle = handle; run.sparesTasks = true; run.stoppedTasks = reuse.stoppedTasks;
        run.children = reuse.children; run.childTasks = reuse.childTasks;
        // Agents spared earlier report into this reply's stream from now on.
        for (const child of run.children.values()) if (child.parentProjection === reuse.topProjection) child.parentProjection = projection;
        await this.applyModelSettings(handle, options);
        if (run.stopped) throw new Error("The response was stopped before its message was sent.");
        run.abort.signal.throwIfAborted();
        if (reuse.permissionMode !== policy.permissionMode) { await query.setPermissionMode(policy.permissionMode); reuse.permissionMode = policy.permissionMode; }
        // Stop can come while the process changes its permission mode.
        if (run.stopped) throw new Error("The response was stopped before its message was sent.");
        run.abort.signal.throwIfAborted();
        if (!handle.commands.optionsMatch()) handle.commands.bind();
        first = reuse.pending; reuse.pending = null; iterator = reuse.iterator;
        run.cuaDriver = reuse.cuaDriver;
        reuse.seen.add(run.turn.id);
        // A Guide message was already written to the process when it was steered.
        if (!run.prompted) input.push(userMessage());
      } else {
        lease = await this.updates.acquire();
        const { sdk } = lease.runtime;
        const tools = advertisedTools(options)
          .filter(spec => spec.name !== "wonder_computer_use")
          .map(spec => sdk.tool(spec.name, spec.description,
          z.fromJSONSchema(spec.inputSchema).shape, async (input) => {
            const callId = live.run.authorizedTools.get(toolCallKey(spec.name, input))?.shift();
            if (!callId) throw new Error("The Cupola tool call could not be correlated with its permission check.");
            if (!live.run.toolResults.has(callId)) live.run.toolResults.set(callId, this.serverCall("item/tool/call", { threadId: session.id, turnId: live.run.turn.id,
              callId, tool: spec.name, arguments: input, agentFamily: "claude" }, live.run.abort.signal));
            const result = await live.run.toolResults.get(callId);
            const response = sdkToolResult(result);
            return response;
          }));
        const model = selectedModel(options.model);
        const project = options.wonderProject;
        const base = baseOptions(lease.runtime);
        // A driver that cannot start leaves the reply without its tools.
        if (cuaDriver(options) && !policy.planMode) {
          // Owned by the reply before it starts, so Stop ends a starting driver.
          const driver = run.cuaDriver = createCuaDriver({ sdk, server: cuaDriver(options), environment: base.env, signal: run.abort.signal });
          try {
            driver.server = await driver.ready;
            if (run.stopped) throw new Error("The response was stopped before its message was sent.");
          } catch {
            driver.stop(); run.cuaDriver = null;
            run.abort.signal.throwIfAborted();
            if (run.stopped) throw new Error("The response was stopped before its message was sent.");
          }
        }
        run.sparesTasks = true;
        handle = { query: null, iterator: null, input, shape: processShape(options, policy), model, permissionMode: policy.permissionMode,
          // Last settings acknowledged by this process.
          settings: { model: options.model, effort: options.effort ?? null, speed: choiceKey(options) },
          abort: run.abort, live, session, lease, draining: false, pending: null, cuaDriver: run.cuaDriver,
          // Transcript entries this process wrote or read live: anything newer
          // in the transcript was written by someone else.
          seen: new Set([run.turn.id]), suggestions: new PromptSuggestions(), sessionState: null,
          // Whether this process keeps file backups for Edit from here.
          checkpointing: options.fileCheckpointing === true };
        run.handle = handle;
        const sdkOptions = { ...base, cwd: options.cwd, model, abortController: run.abort,
          mcpServers: { ...(tools.length ? { wonder: sdk.createSdkMcpServer({ name: "wonder", version: "1.0.0", tools }) } : {}),
            ...(run.cuaDriver ? { cua_driver: run.cuaDriver.server } : {}) },
          canUseTool: (name, input, context) => this.permission(session, live.run, live.policy, name, input, context),
          hooks: { PreToolUse: [{ hooks: [(input) => live.policy.beforeTool(input)] }],
            UserPromptSubmit: [{ hooks: [async input => promptContext(live, input)] }],
            SessionStart: [{ hooks: [async input => {
              if (input.hook_event_name === "SessionStart" && input.source === "clear" && input.agent_id == null)
                await this.recordNativeReset(session, handle, { sessionId: input.session_id });
              return {};
            }] }] },
          permissionMode: policy.permissionMode, sandbox: policy.sandbox(), includePartialMessages: true,
          // Ordinary tasks run until completion, cancellation, or the subscription limit.
          // A fixed tool-turn cap otherwise abandons valid long-running work.
          persistSession: true, verbatimPrompts: true,
          perTaskStopAffordance: true, promptSuggestions: true,
          settings: { ...base.settings, ...modelSettings(options, false), enforceAvailableModels: true },
          ...(sdkEffort(options) ? { effort: sdkEffort(options) } : {}),
          ...(options.outputSchema ? { outputFormat: { type: "json_schema", schema: options.outputSchema } } : {}),
          ...(session.sdkStarted ? { resume: session.sdkSessionId } : { sessionId: session.sdkSessionId }),
          ...(options.fileCheckpointing === true ? { enableFileCheckpointing: true } : {}) };
        // After Edit from here the session continues from before the owner's
        // message: Claude Code drops it and everything after from its context.
        if (sdkOptions.resume && session.rewind) { sdkOptions.resumeSessionAt = session.rewind.at; run.rewound = session.rewind; }
        // A killed process can have persisted an SDK transcript before our init
        // event was saved. Query the exact owned UUID before deciding to resume.
        if (!session.sdkStarted && typeof sdk.getSessionInfo === "function") {
          if (await sdk.getSessionInfo(session.sdkSessionId, { dir: options.cwd })) {
            delete sdkOptions.sessionId; sdkOptions.resume = session.sdkSessionId; session.sdkStarted = true;
          }
        }
        // SDK 0.3.283 documents first-turn suppression. Only an actually new
        // native session proves that; a resumed session's eligibility is unknown.
        suggestionSuppressed ||= Boolean(sdkOptions.sessionId);
        {
          // Normal Claude Code behavior and project configuration (none in Read
          // only), still bounded by Wonder's PreToolUse/canUseTool policy and
          // the command boundary or Claude Code's sandbox.
          const append = [...(wonderPreamble(options) ? [wonderPreamble(options)] : []),
            process.platform === "win32" ? WINDOWS_COMMAND_GUIDANCE : policy.unsandboxed ? NESTED_SANDBOX_GUIDANCE : SANDBOX_GUIDANCE,
            ...(wonderInstruction(options) ? [wonderInstruction(options)] : [])];
          // Before the owner trusts the folder's own configuration, only the
          // owner's settings load, and MCP servers only from Cupola.
          const trusted = options.folderTrust?.trusted === true;
          const settingSources = trusted ? policy.settingSources : policy.settingSources.filter(source => source === "user");
          // MCP is gated on its own: an untrusted folder with its own tool
          // servers starts only Cupola's, which also leaves out connectors.
          const strictMcp = !settingSources.length || (!trusted && options.folderTrust?.servers !== false);
          Object.assign(sdkOptions, { systemPrompt: { type: "preset", preset: "claude_code", ...(append.length ? { append: append.join("\n\n") } : {}) },
            settingSources, additionalDirectories: project.additionalDirectories,
            // The Skill tool loads the owner's skills that Claude Code discovers
            // from these setting sources, as in Claude Code itself (a folder's
            // own skills only once the owner trusts it).
            // Planning ends by proposing its plan through ExitPlanMode, which the
            // policy turns into a plan for the owner to review in Wonder.
            tools: [...BUILTINS, "Glob", "Grep", ...(policy.planMode ? ["ExitPlanMode"] : [])], strictMcpConfig: strictMcp,
            // The owner's claude.ai connectors, listed as usable in Cupola, load
            // as in Claude Code; each call asks per the chat's access
            // (permissions.mjs). Read only loads no MCP configuration at all.
            settings: { ...sdkOptions.settings, disableClaudeAiConnectors: strictMcp,
              ...(trusted ? {} : { enableAllProjectMcpServers: false }) } });
          // Cupola's pack folder holds only its enabled skills; the policy
          // refuses any other Wonder skill.
          const pack = wonderSkills(options);
          if (pack) sdkOptions.plugins = [{ type: "local", path: pack.plugin, skipMcpDiscovery: true }];
        }
        query = sdk.query({ prompt: (async function* () {
          await submit.promise;
          // Project turns name their transcript entry after the Wonder turn so
          // native history and live events reconcile by the same identity.
          if (!run.abort.signal.aborted) yield userMessage();
          // Later replies join this open stream until the process is closed.
          for (let next = await input.next(); next; next = await input.next()) yield next;
        })(), options: sdkOptions });
        run.query = query;
        await query.initializationResult();
        if (!accessKind(await query.accountInfo(), sdkOptions.env)) throw new Error("Sign in to Claude or configure an API key on your computer before using this chat.");
      }
      submit.resolve();
      // Iterate by hand: leaving a `for await` calls return(), which would end
      // the query that a soft interrupt keeps alive for its spared agents.
      if (!reuse) {
        iterator = query[Symbol.asyncIterator](); handle.query = query; handle.iterator = iterator;
        handle.commands = new SessionCommands(session, handle, () => this.processHandle(session) === handle);
      }
      for (;;) {
        const next = await (first ?? iterator.next());
        first = null;
        if (next.done) break;
        const message = next.value;
        // An interrupted reply ends at the SDK's result for the interrupt, which
        // is not a failure; running agents are left to their own Stop.
        if (run.interrupting && message.type === "result") { projection.finish("interrupted"); await flush(); break; }
        handle.commands.observe(message);
        await this.observeNativeReset(session, handle, message);
        this.observeContextUsage(session, handle, message);
        await this.observeSessionState(session, handle, message);
        observeTasks(run.roster, message, session.sdkSessionId);
        if (message.uuid && ["user", "assistant"].includes(message.type)) handle?.seen?.add(message.uuid);
        // Claude answering means the new branch is in the transcript.
        if (run.rewound && session.rewind === run.rewound && message.type === "assistant" && !message.parent_tool_use_id) {
          delete session.rewind; run.rewound = null; await this.sessions.save(session);
        }
        if (message.type === "result") run.answered = [message.user_message_uuid, ...(message.user_message_uuids ?? [])].filter(Boolean);
        projection.accept(message);
        if (message.type === "result") {
          const answered = [message.user_message_uuid, ...(message.user_message_uuids ?? [])].filter(Boolean);
          if (projection.terminal && new Set(answered).size === 1 && answered.includes(run.turn.id) && !run.steeredBy) {
            if (!suggestionSuppressed) handle.suggestions.complete(run.turn.id, session.sdkSessionId, projection.result);
          }
          else handle.suggestions.invalidate();
        }
        if (message.type === "system" && message.subtype === "init") await this.sessions.save(session);
        while (run.childMessages.length) await this.child(session, run, run.childMessages.shift(), projection);
        await flush();
        if (projection.terminal) break;
      }
      // Stopped by the owner, the reply ends without an error.
      if (!projection.terminal) projection.finish(run.stopped ? "interrupted" : "failed", run.yielded ?? (run.stopped ? undefined : "Claude stopped before returning a complete response."));
    } catch (error) {
      projection.finish(run.stopped ? "interrupted" : "failed", run.yielded ?? (run.stopped ? undefined : error.message));
    } finally {
      clearInterval(watch);
      submit.resolve();
      // No Guide is admitted from here on; one already being saved joins first.
      run.finalizing = true;
      await run.admitting?.catch(() => {});
      // A soft interrupt leaves the process (and the agents it spared) running.
      // A reply that joined a surviving process keeps it while agents still run.
      let spared = Boolean(handle && handle.query === query && !run.abort.signal.aborted && !run.hardStop);
      // A Project reply ends at its result; its process stays up for the next
      // reply and for background agents, commands or monitors still running,
      // as in Claude on the Mac. A failed reply keeps it only for those.
      const steer = run.steeredBy;
      if (spared && !run.interrupting) {
        spared = Boolean(projection.terminal && run.sparesTasks);
        if (spared && projection.result?.status === "failed") {
          try { spared = run.roster.tasks?.size > 0 || (await this.projectTasks(session)).some(task => task.status === "running"); } catch { spared = false; }
        }
        // Claude on the Mac opened the chat meanwhile: it continues from here.
        if (spared && !steer && await this.openOnMac(session)) spared = false;
      }
      // A steered reply hands its process straight to the Guide turn.
      if (spared) { this.linger(session, run, handle, lease, projection, !steer); lease = null; }
      else {
        done.resolve(); query?.close(); handle?.abort.abort(); run.abort.abort();
        // The process ends, so background commands and monitors it ran end too.
        projection.settleAllBackground("interrupted", undefined, [session.id, ...[...run.children.values()].map(child => child.session.id)]);
      }
      if (!spared) await run.cuaDriver?.close();
      // Finish descendants first so each owner's durable snapshot includes its
      // children's final state, including when the whole query is interrupted.
      for (const child of [...run.children.values()].reverse()) {
        // A spared agent is still running: its own notification finishes its card.
        if (!spared && !child.projection.terminal) child.projection.finish(run.stopped ? "interrupted" : "failed", "The parent response ended before this task confirmed completion.");
        await child.flush();
      }
      projection.result.items = [...run.turn.items.filter(i => i.type === "userMessage"), ...projection.result.items];
      Object.assign(run.turn, projection.result);
      await this.sessions.save(session);
      // The Guide turn stays owned while it is handed on (see closeStaleTurns).
      if (steer) this.handoffs.add(steer.turn.id);
      await flush(true);
      await this.finishNativeReset(session).catch(() => {});
      this.active.delete(session.id);
      await this.publishSessionStatus(session, handle);
      try {
        await lease?.release();
        if (steer) await this.startSteered(session, steer, spared ? handle : null, options, policy, run.answered?.includes(steer.turn.id), run.stopped);
      } finally { if (steer) this.handoffs.delete(steer.turn.id); }
    }
  }
  // A turn the journal still marks running that no run here owns ended with
  // the process that ran it: Claude stops with its process, and nothing will
  // report its end. Record it as interrupted and report it, so it neither
  // shows as running nor refuses the next message.
  async closeStaleTurns(session) {
    if (session.parent) return;
    await this.admitting.get(session.id); // A turn being admitted belongs to its run.
    const run = this.active.get(session.id);
    await run?.admitting?.catch(() => {}); // A Guide being saved belongs to its reply.
    const owned = new Set([run?.turn.id, run?.steeredBy?.turn.id, ...this.handoffs].filter(Boolean));
    const stale = session.turns.filter(t => t.status === "inProgress" && !owned.has(t.id));
    if (!stale.length) return;
    for (const turn of stale) Object.assign(turn, { status: "interrupted", error: { message: STALE_TURN } });
    await this.sessions.save(session);
    for (const turn of stale) await this.send({ method: "turn/completed", params: { threadId: session.id, turnId: turn.id,
      turn: { id: turn.id, status: turn.status, items: [], error: turn.error }, agentFamily: "claude" } });
  }
  async yieldToMac(session, run, startedAt) {
    if (run.checkingMac || run.stopped || this.active.get(session.id) !== run) return;
    run.checkingMac = true;
    try {
      const mac = await claudeSessionState(session.sdkSessionId, this.claudeSessionsDir);
      if (!(mac.busySince > startedAt) || run.stopped || this.active.get(session.id) !== run) return;
      run.yielded = "You continued this conversation in Claude on your computer, so Cupola stopped its reply.";
      run.stopped = true; run.hardStop = true; run.abort.abort(); run.query?.close();
    } catch {} finally { run.checkingMac = false; }
  }
  // Runs a Guide turn in the process its steered reply left. Claude may have
  // answered it within that reply (both prompts in one result); without the
  // process it cannot be answered and fails honestly.
  async startSteered(session, steer, handle, options, policy, absorbed, stopped = false) {
    handle?.suggestions.invalidate();
    if (!handle || absorbed) {
      const output = [];
      const projection = new TurnProjection({ threadId: session.id, turnId: steer.turn.id, emit: event => output.push(event) });
      projection.start();
      // Stopped by the owner, the Guide ends with the reply it was sent into.
      if (handle) projection.finish("completed");
      else if (stopped) projection.finish("interrupted");
      else projection.finish("failed", "Claude stopped before it could take this message. Send it again.");
      projection.result.items = [...steer.turn.items.filter(i => i.type === "userMessage"), ...projection.result.items];
      Object.assign(steer.turn, projection.result);
      await this.sessions.save(session);
      for (const event of output) await this.send(event);
      if (handle) this.linger(session, { childMessages: handle.childMessages, stoppedTasks: handle.stoppedTasks, children: handle.children,
        childTasks: handle.childTasks, roster: handle.roster }, handle, handle.lease, handle.topProjection);
      return;
    }
    const run = { reuse: handle, turn: steer.turn, policy, prompted: true, abort: new AbortController(), query: null, children: new Map(), childTasks: new Map(),
      stopped: false, authorizedTools: new Map(), toolResults: new Map(), roster: handle.roster };
    handle.seen.add(steer.turn.id);
    this.active.set(session.id, run);
    run.finished = new Promise((resolve, reject) => setImmediate(() => { this.execute(session, run, options, policy, steer.content).then(resolve, reject); }));
    run.finished.catch(() => this.onFatal());
  }
  // After a soft interrupt the Claude process stays up so spared agents keep
  // running and stay stoppable. The next reply joins it (see turn/start). It
  // ends when no task is left running, when a reply needs a differently shaped
  // process (model, effort, folders, tools or sandbox change), on fork, or when
  // Wonder closes; each of those ends any agent still running in it.
  linger(session, run, handle, lease, projection, drain = true) {
    Object.assign(handle, { lease, topProjection: projection, childMessages: run.childMessages, stoppedTasks: run.stoppedTasks, children: run.children, childTasks: run.childTasks, roster: run.roster,
      draining: drain, lingeredAt: Date.now(), lastActive: Date.now(), finishedTurn: run.finished });
    if (!drain) return;
    const drainOwner = Symbol(); handle.drainOwner = drainOwner;
    const ownsDrain = () => handle.draining && handle.drainOwner === drainOwner;
    this.lingering.set(session.id, handle);
    this.evictWarm(session.id).catch(() => {});
    // When Claude on the Mac (the desktop app or a terminal) opens this chat,
    // it continues from the transcript in its own process. This one yields at
    // once, ending the work it kept, so two processes never write one session:
    // its next notice or reply would fork the chat.
    handle.watch = setInterval(async () => {
      if (handle.checking || this.lingering.get(session.id) !== handle) return;
      handle.checking = true;
      try {
        // Idle too long, or holding background work past its bound. An idle
        // one also ends for a staged SDK update, which waits for every process.
        const held = Date.now() - handle.lingeredAt > this.backgroundHoldMs;
        const idle = Date.now() - handle.lastActive > this.warmIdleMs || Boolean(this.updates.pending);
        const expired = !this.processBusy(handle) && (held || idle) && (await this.lingerHasTasks(session, handle) ? held : idle);
        const macOpened = await this.openOnMac(session);
        if ((expired || macOpened) && this.lingering.get(session.id) === handle) await this.endLinger(session.id, true, !macOpened);
      }
      catch {} finally { handle.checking = false; }
    }, this.yieldCheckMs);
    handle.watch.unref?.();
    (async () => {
      try {
        while (ownsDrain()) {
          // A new reply adopts this pending read instead of racing it.
          handle.pending ??= handle.iterator.next();
          const next = await handle.pending;
          if (!ownsDrain()) return;
          handle.pending = null;
          if (next.done) break;
          const message = next.value;
          handle.commands.observe(message);
          if (message.type === "system" && message.subtype === "commands_changed") continue;
          handle.lastActive = Date.now();
          await this.observeNativeReset(session, handle, message);
          if (session.pendingReset) {
            await handle.finishedTurn;
            await this.finishNativeReset(session).catch(() => {});
            continue;
          }
          this.observeContextUsage(session, handle, message);
          if (message.type === "system" && message.subtype === "session_state_changed") {
            await this.observeSessionState(session, handle, message);
            continue;
          }
          if (message.type === "prompt_suggestion") {
            // Completion is durably published first; a next Send never waits
            // for this optional item and invalidates its process fence.
            await handle.finishedTurn;
            if (!ownsDrain() || this.lingering.get(session.id) !== handle) return;
            const hint = handle.suggestions.take(message, session.turns.at(-1)?.id);
            const turn = hint && session.turns.find(turn => turn.id === hint.turnId && turn.status === "completed");
            if (turn) {
              turn.items.push(hint.item);
              await this.sessions.save(session);
              await this.send({ method: "item/completed", params: { threadId: session.id, turnId: turn.id,
                item: hint.item, agentFamily: "claude" } });
            }
            continue;
          }
          if (message.uuid && ["user", "assistant"].includes(message.type)) handle.seen.add(message.uuid);
          observeTasks(handle.roster, message, session.sdkSessionId);
          // The old turn and its agents belong to the cleared native context.
          if (!session.turns.some(turn => turn.id === handle.topProjection.turnId)) continue;
          // A background command or monitor that ends while Claude is idle
          // settles the card its turn left running.
          if (message.type === "system" && !message.parent_tool_use_id && ["task_notification", "task_updated"].includes(message.subtype)) {
            try { await this.settleBackground(session, handle, projection => projection.taskEvent(message)); } catch {}
          }
          // Spared agents keep reporting: their own cards follow them.
          if (message.parent_tool_use_id || (message.type === "system" && ["task_started", "task_progress", "task_notification"].includes(message.subtype))) {
            try {
              await this.child(session, handle, message, handle.topProjection);
              while (handle.childMessages.length) await this.child(session, handle, handle.childMessages.shift(), handle.topProjection);
            } catch {}
          } else if (["stream_event", "assistant", "user", "result"].includes(message.type)) {
            handle.suggestions.invalidate();
            // When a background task finishes, Claude replies on its own in this
            // process. History adds that reply to the last turn, so it streams
            // into the same turn here; the turn stays ended.
            try { await this.continueTurn(session, handle, message); } catch {}
          }
          // With nothing left running the process stays warm for the next
          // reply; the watch above ends it when idle too long.
        }
      } catch {}
      // Nothing awaits this block: a teardown error must not end the runtime.
      if (ownsDrain() && this.lingering.get(session.id) === handle) await this.endLinger(session.id).catch(() => {});
    })();
  }
  // A live interactive Claude Code process (not an Agent SDK run like this
  // one) has the session open.
  // The session's task list as Claude Code saved it, so a reply that updates a
  // task made in an earlier reply shows the whole list. Unreadable: empty.
  async taskList(session) {
    if (!session.sdkStarted || !/^[0-9a-f-]{36}$/i.test(session.sdkSessionId ?? "")) return new ClaudeTaskList();
    const dir = join(this.taskListsDir, session.sdkSessionId);
    try {
      const names = (await readdir(dir)).filter(name => /^[\w-]{1,64}\.json$/.test(name)).slice(0, 200);
      const tasks = await Promise.all(names.map(name => readFile(join(dir, name), "utf8").then(JSON.parse).catch(() => null)));
      return new ClaudeTaskList(tasks.filter(task => task && typeof task === "object"));
    } catch { return new ClaudeTaskList(); }
  }
  async openOnMac(session) {
    return (await claudeSessionState(session.sdkSessionId, this.claudeSessionsDir)).open === true;
  }
  // Settles background commands and monitors outside a running reply: the
  // turn that launched them ended, so a short-lived projection publishes each
  // completion under that turn. `settle` runs against it and says whether
  // anything changed; the turn's saved items are the same objects it settles.
  async settleBackground(session, handle, settle) {
    if (!session.turns.some(turn => turn.id === handle.topProjection.turnId)) return;
    const output = [];
    const projection = new TurnProjection({ threadId: session.id, turnId: handle.topProjection.turnId,
      emit: event => output.push(event), continuation: true });
    if (settle(projection)) await this.sessions.save(session);
    while (output.length) await this.send(output.shift());
  }
  async continueTurn(session, handle, message) {
    if (!session.turns.some(turn => turn.id === handle.topProjection.turnId)) return;
    if (!handle.followUp) {
      if (message.type === "result") return;
      // The completed turn can receive a background-task continuation. Its
      // previous hint no longer describes the latest reply; the new item
      // hides it live, and removing the receipt keeps hydration from reviving it.
      const turn = session.turns.find(turn => turn.id === handle.topProjection.turnId);
      if (turn?.items.some(item => item.type === "promptSuggestion")) {
        turn.items = turn.items.filter(item => item.type !== "promptSuggestion");
        await this.sessions.save(session);
      }
      const output = [];
      // A task report wakes Claude into a run of its own, as history shows it:
      // the report's row, then the run's work and reply, under the report's
      // ID. Like the continuation it replaces, it sends no turn start or end,
      // so a wake never raises attention or a push by itself.
      const report = message.type === "user" && !message.parent_tool_use_id && message.uuid ? taskNotification(message) : null;
      const wake = report && { id: message.uuid, status: "completed", error: null, items: [wakeItem(message, report)] };
      if (wake && !session.turns.some(turn => turn.id === wake.id)) {
        session.turns.push(wake);
        await this.sessions.save(session);
        output.push({ method: "item/completed", params: { threadId: session.id, turnId: wake.id, item: wake.items[0], agentFamily: "claude" } });
      }
      handle.followUp = { output, projection: new TurnProjection({ threadId: session.id, turnId: wake?.id ?? handle.topProjection.turnId,
        tasks: handle.topProjection.tasks, emit: event => output.push(event), onChild: child => handle.childMessages.push(child), continuation: true }) };
    }
    const { projection, output } = handle.followUp;
    projection.accept(message);
    if (message.type === "result") { projection.finish("completed"); handle.followUp = null; }
    while (handle.childMessages.length) await this.child(session, handle, handle.childMessages.shift(), handle.topProjection);
    while (output.length) await this.send(output.shift());
  }
  unlinger(sessionId, handle) {
    this.lingering.delete(sessionId); handle.draining = false; clearInterval(handle.watch);
  }
  async runsNothingElsewhere(session) {
    if (!session.sdkStarted || this.active.has(session.id) || this.lingering.has(session.id)) return true;
    return (await claudeSessionState(session.sdkSessionId, this.claudeSessionsDir)).open === false;
  }
  async lingerHasTasks(session, handle) {
    if (handle.roster?.tasks?.size > 0) return true;
    try { return (await this.projectTasks(session)).some(task => task.status === "running"); } catch { return true; }
  }
  // Someone else (Claude on the Mac, a terminal, another Agent SDK app) wrote
  // to the transcript since this process last did.
  async transcriptMoved(session, handle) {
    const project = session.options.wonderProject;
    if (!project || !session.sdkStarted) return false;
    const latest = await this.sessionFiles.latestMessageUuid(session.sdkSessionId, project.cwd).catch(() => null);
    return Boolean(latest && !handle.seen?.has(latest));
  }
  // Keeps at most `maxWarm` idle processes; ones running background work stay.
  // Background work is judged by lingerHasTasks, as for reconnecting and expiry.
  async evictWarm(keep) {
    const idle = [];
    for (const [id, h] of this.lingering) if (id !== keep && !this.processBusy(h) && !await this.lingerHasTasks(h.session, h)) idle.push([id, h]);
    idle.sort((a, b) => a[1].lastActive - b[1].lastActive);
    while (idle.length >= this.maxWarm) {
      const [id, h] = idle.shift();
      // Only the process judged idle above ends, not one that replaced it meanwhile.
      if (this.lingering.get(id) === h) await this.endLinger(id, true, true).catch(() => {});
    }
  }
  async endLinger(sessionId, lookup = true, requireIdle = false) {
    const handle = this.lingering.get(sessionId);
    if (!handle) return;
    let tasks = [];
    if (lookup) try { tasks = await this.projectTasks(handle.session); } catch {}
    if (this.lingering.get(sessionId) !== handle) return;
    if (requireIdle && this.processBusy(handle)) throw new InputRefusal(RUNNING_REFUSAL, { action: "wait" });
    // Keep the journal object cached through teardown: its settle notices
    // save this exact object, which eviction would make a stale owner.
    const release = this.sessions.retain(sessionId);
    try { await this.finishLinger(sessionId, handle, tasks); } finally { release(); }
  }
  async finishLinger(sessionId, handle, tasks) {
    this.lingering.delete(sessionId); handle.draining = false; clearInterval(handle.watch);
    // Each step runs whatever an earlier one threw: a failed save (disk full,
    // a reloaded journal) must not leave the process running unowned or its
    // SDK lease held, which keeps a staged update from activating.
    const step = async run => { try { await run(); } catch {} };
    // Agents shown as running end with the process; report what the transcript knows.
    for (const [toolId, child] of [...(handle.children ?? [])].reverse()) await step(async () => {
      if (!child.projection.terminal) {
        const status = tasks.find(task => task.id === toolId)?.status;
        const outcome = status === "completed" || status === "failed" ? status : "interrupted";
        child.projection.finish(outcome, outcome === "failed" ? "The agent task failed." : undefined);
      }
      await child.flush();
    });
    // Background commands and monitors end with the process too: the
    // transcript's outcome where it has one, otherwise interrupted.
    await step(() => this.settleBackground(handle.session, handle, projection => settleEndedBackground(projection, tasks, handle)));
    await step(() => handle.input.close());
    await step(() => handle.query.close());
    await step(() => handle.abort.abort());
    await step(() => handle.cuaDriver?.close());
    await step(() => handle.lease?.release());
    await step(() => this.publishSessionStatus(handle.session, handle));
  }
  async child(parent, run, message, parentProjection) {
    const taskId = message.task_id ?? message.result?.agentId;
    const toolId = message.parent_tool_use_id ?? message.tool_use_id ?? run.childTasks.get(taskId)
      ?? (message.subtype === "task_started" && taskId ? `task:${taskId}` : null);
    if (!toolId || message.ambient || message.skip_transcript) return;
    let child = run.children.get(toolId);
    if (!child) {
      // Progress and completion events also belong to shell/MCP/housekeeping
      // tasks. Only a visible local-agent start establishes child ownership.
      if (message.type !== "system" || message.subtype !== "task_started" || message.task_type !== "local_agent") {
        if (message.type === "agent_result" && message.is_error) {
          const call = parentProjection.agentTools.get(toolId);
          const item = parentProjection.startItem({ type: "subAgentActivity", id: toolId,
            agentNickname: call?.input?.description ?? "Agent", kind: "failed", status: "inProgress", success: false,
            error: { message: typeof message.content === "string" ? message.content.slice(0, 2000) : "The agent could not start." } });
          parentProjection.finishItem(item);
        }
        return;
      }
      const owner = [...run.children.values()].find(c => c.projection.agentTools.has(toolId));
      if (owner) { parent = owner.session; parentProjection = owner.projection; }
      const expectedDepth = (parent.parent?.depth ?? 0) + 1;
      const depth = message.spawn_depth ?? expectedDepth;
      if (!Number.isInteger(depth) || depth !== expectedDepth || depth > 16) return;
      const session = await this.sessions.create(parent.options, { threadId: parent.id, nativeSessionId: parent.sdkSessionId, depth,
        name: String(message.description ?? "Helper").slice(0, 100), role: message.subagent_type ?? "Helper" });
      const turn = { id: randomUUID(), status: "inProgress", items: [] }; session.turns.push(turn);
      const output = [], childMessages = run.childMessages;
      const projection = new TurnProjection({ threadId: session.id, turnId: turn.id, emit: event => output.push(event),
        onChild: message => childMessages.push(message) });
      const activity = { type: "subAgentActivity", id: toolId, agentThreadId: session.id,
        agentNickname: session.parent.name, agentRole: session.parent.role, kind: "started", status: "running" };
      child = { session, turn, projection, parentProjection, flush: async () => {
        const terminal = output.findLast(e => e.method === "turn/completed")?.params.turn;
        // History reads while the task is running must see the same items as
        // the stream. Persist once at completion, not once per text delta.
        if (turn.items.length !== projection.items.size) turn.items = [...projection.items.values()];
        if (terminal) {
          Object.assign(turn, terminal); await this.sessions.save(session);
          if (taskId && run.roster?.updates?.has(taskId))
            run.roster.updates.set(taskId, { status: terminal.status, progressSummary: null });
          activity.status = terminal.status; activity.kind = terminal.status;
          if (terminal.error) activity.error = terminal.error;
          child.parentProjection.notify("item/completed", { item: { ...activity } });
        }
        while (output.length) await this.send(output.shift());
        await owner?.flush();
      } };
      run.children.set(toolId, child);
      if (taskId) run.childTasks.set(taskId, toolId);
      await this.sessions.save(session);
      await this.send({ method: "thread/started", params: { thread: this.describe(session), agentFamily: "claude" } });
      parentProjection.startItem(activity);
      projection.start();
    }
    if (message.parent_tool_use_id) child.projection.accept({ ...message, parent_tool_use_id: null });
    const completedResult = message.type === "agent_result" && !message.is_error && message.result?.status === "completed";
    if (message.subtype === "task_notification" || completedResult || (message.type === "agent_result" && message.is_error)) {
      if (!child.projection.terminal) {
        const status = completedResult ? "completed" : message.status === "completed" ? "completed" : message.status === "stopped" ? "interrupted" : "failed";
        const summary = completedResult ? message.result.content?.filter(b => b.type === "text").map(b => b.text).join("\n\n") : message.summary;
        // Foreground agents sometimes emit only the task report, without child
        // assistant frames. Never parse the model-directed tool-result trailer.
        if (typeof summary === "string" && summary.trim() && ![...child.projection.items.values()].some(i => i.type === "agentMessage" && i.text.trim() === summary.trim()))
          child.projection.completeText(`task:${toolId}:result`, summary);
        child.projection.finish(status, status === "failed" ? summary ?? "The agent task failed." : undefined);
      }
    }
    await child.flush();
  }
  async close() {
    this.stopActivated?.();
    for (const run of this.active.values()) { run.stopped = true; run.abort.abort(); run.query?.close(); }
    await Promise.allSettled([...this.active.values()].map(run => run.finished));
    for (const id of [...this.lingering.keys()]) await this.endLinger(id, false);
    for (const pending of this.pending.values()) { pending.cleanup(); pending.reject(new Error("Cupola closed the Claude runtime.")); }
    this.pending.clear();
    await this.readers.close(); await this.sessionFiles.close(); await this.sessions.readers.close();
    this.transcripts.clear(); this.taskProjections.clear(); this.sessions.evict();
  }
}

// What the installed OpenCode offers: its version and the models its own
// configuration lists. Both come from OpenCode's CLI; nothing is installed.
import { execFile } from "node:child_process";
import { access, constants, lstat, readFile, readdir, realpath, rm, stat } from "node:fs/promises";
import { homedir } from "node:os";
import { delimiter, join, resolve } from "node:path";
import { setTimeout as sleep } from "node:timers/promises";
import { pathToFileURL } from "node:url";
import { FAMILY } from "./projection.mjs";

export function nativeModel(value) {
  if (typeof value !== "string" || !value.startsWith(`${FAMILY}:`) || !/^[\w.@-]+\/[\w.:@/+-]{1,200}$/.test(value.slice(FAMILY.length + 1)))
    throw new Error("Choose an OpenCode model for this conversation.");
  return value.slice(FAMILY.length + 1);
}

// Work mode and permission policy are separate questions with separate owners.
//
// A work mode picks how the agent approaches a turn: OpenCode's mode selects
// one of its agents, and Cursor's modes include Ask, a question-and-answer
// chat. Neither is a permission level. Plan is not read-only either: under
// OpenCode's default rules its plan agent can still run shell commands. The
// values are native to each agent, not ACP constants; Cupola selects only
// these two and never Cursor's Ask.
//
// Permission policy stays in each agent's own settings. ACP and these agents
// offer no per-session permission setter, and Cupola does not invent one
// (launch flags, environment or a rewritten config would reach every chat the
// process or folder serves). The neutral stored values pass and make no access
// guarantee; any other access or approval choice fails before work rather
// than being approximated with a work mode.
const PERMISSION_FIELDS = { mode: "workspace", approvalMode: "ask" };

// A permission profile says what a chat's permissions stand for and who
// enforces them, as the host's `PermissionProfile` and `ApprovalEnforcement`
// do (crates/wonder-store/src/provider.rs). `native`: the agent's own settings
// decide, and an approval card appears only for an action the agent itself
// asks about, so the stored "ask" is never an enforced Ask. The neutral
// values older hosts stored, and their absence, are `native`, so existing
// chats keep the agent's own behaviour.
export const PERMISSION_PROFILES = Object.freeze({
  native: Object.freeze({ id: "native", enforcement: "native" }),
});

export function workMode(adapter, policy) {
  const plan = policy?.planMode ?? false;
  if (typeof plan !== "boolean") throw new Error("Turn Plan on or off for this conversation.");
  return adapter.workModes[plan ? "plan" : "default"];
}

export function permissionProfile(adapter, policy) {
  if (policy != null && (typeof policy !== "object" || Array.isArray(policy)))
    throw new Error(`Change permissions in ${adapter.label} settings on your computer.`);
  for (const [field, neutral] of Object.entries(PERMISSION_FIELDS))
    if (policy?.[field] != null && policy[field] !== neutral)
      throw new Error(`Change permissions in ${adapter.label} settings on your computer.`);
  return adapter.permissionProfiles[0];
}

// Each adapter lists the profiles it can honour; OpenCode and Cursor have
// only `native`. Approval cards appear only when the agent itself asks.
const withPolicy = adapter => Object.freeze(Object.assign(adapter, {
  permissionProfiles: Object.freeze([PERMISSION_PROFILES.native]), workModes: Object.freeze(adapter.workModes),
  permissionProfile: policy => permissionProfile(adapter, policy),
  validatePolicy: policy => { permissionProfile(adapter, policy); workMode(adapter, policy); },
  sessionSelections: (model, policy) => [["model", model], ["mode", workMode(adapter, policy)]],
}));

// OpenCode 1.18.27's command path omits file parts (including for /compact
// and unknown commands). ACP advertises no capability for this combination.
// Keep the restriction with OpenCode's adapter, not the shared ACP transport.
export function validatePromptComposition(blocks) {
  if (blocks.every(block => block.type === "text")) return;
  // Match native text ordering: embedded text resources become labelled text,
  // while images and binary resources do not participate in slash recognition.
  const text = blocks.map(block => block.type === "text" ? block.text
    : block.type === "resource" && typeof block.resource.text === "string"
      ? `[${block.resource.uri}]\n${block.resource.text}` : "").join("").trim();
  if (/^\/\S/.test(text))
    throw new Error("OpenCode cannot combine slash commands with attachments. Send the command and attachments separately.");
}

const SEARCH = ["/opt/homebrew/bin", "/usr/local/bin", join(homedir(), ".opencode", "bin"), join(homedir(), ".local", "bin")];

// Windows: a native `opencode.exe`, or npm's global install, whose shims
// run the package's Node script `bin/opencode`. Wonder runs that script with
// Node itself rather than through cmd.exe, so stopping it ends OpenCode, not
// only a shell wrapper. npm's extensionless `opencode` shim is a POSIX shell
// script, so it is never a candidate.
const WINDOWS_NAMES = ["opencode.exe", join("node_modules", "opencode-ai", "bin", "opencode")];

// Launch arguments the owner saved for this provider: a JSON array of strings in
// WONDER_OPENCODE_ARGS or WONDER_CURSOR_ARGS, set by the host at launch.
export function ownerArguments(provider, env = process.env) {
  const name = provider === "cursor" ? "WONDER_CURSOR_ARGS" : "WONDER_OPENCODE_ARGS";
  try {
    const list = JSON.parse(env[name] ?? "[]");
    return Array.isArray(list) ? list.filter(item => typeof item === "string") : [];
  } catch { return []; }
}

export async function locateOpenCode(env = process.env, platform = process.platform) {
  const names = platform === "win32" ? WINDOWS_NAMES : ["opencode"];
  const candidates = env.WONDER_OPENCODE_BIN ? [env.WONDER_OPENCODE_BIN]
    : [...(env.PATH ?? "").split(delimiter).filter(Boolean), ...(platform === "win32" ? [] : SEARCH)]
      .flatMap(dir => names.map(name => join(dir, name)));
  for (const path of candidates) {
    try { await access(path, constants.X_OK); return path; } catch {}
  }
  return null;
}

// How to start a located OpenCode: Windows cannot execute a Node script by
// its shebang, so anything but an `.exe` runs under this Node.
export function openCodeCommand(binary, args, platform = process.platform) {
  return platform === "win32" && !/\.exe$/i.test(binary)
    ? { command: process.execPath, args: [binary, ...args] } : { command: binary, args };
}

function run(binary, args, timeout = 30_000, signal) {
  signal?.throwIfAborted();
  const { command, args: argv } = openCodeCommand(binary, args);
  return new Promise((resolve, reject) => {
    let aborted = false;
    // execFile's AbortSignal can settle before the child exits, and on the
    // installed Node does not honor killSignal for abort. Own cancellation
    // explicitly and wait for execFile's exit callback before returning.
    const abort = () => {
      aborted = true;
      if (child.exitCode == null && child.signalCode == null) child.kill("SIGKILL");
    };
    const child = execFile(command, argv, { timeout, killSignal: "SIGKILL", maxBuffer: 16 * 1024 * 1024 }, (error, stdout, stderr) => {
      signal?.removeEventListener("abort", abort);
      if (aborted) reject(signal.reason);
      else if (error) reject(Object.assign(error, { stderr: String(stderr ?? "").slice(-4000) }));
      else resolve(stdout);
    });
    signal?.addEventListener("abort", abort, { once: true });
    if (signal?.aborted) abort();
  });
}

// OpenCode keeps its state in SQLite. Two of its commands opening it at once
// can fail with "database is locked" (measured on 1.19.3, before 2.x moved
// the listing onto a shared service; kept as a guard).
const LOCKED = /database is locked|SQLITE_BUSY/i;
export const databaseLocked = text => LOCKED.test(String(text ?? ""));

// The bridge runs these one at a time, but it cannot exclude OpenCode's own
// app or another terminal: retry a classified lock failure once, nothing else.
async function runOpenCode(binary, args, timeout, signal) {
  try { return await run(binary, args, timeout, signal); } catch (error) {
    if (signal?.aborted || !databaseLocked(error.stderr)) throw error;
    await sleep(250 + Math.floor(Math.random() * 250), undefined, { signal });
    return run(binary, args, timeout, signal);
  }
}

// A short reason for the host log. Never the command's output: `debug config`
// prints the whole configuration, secrets included.
function probeFailure(error, { stderr = true } = {}) {
  if (error?.killed || error?.signal) return "timed out";
  const line = stderr ? String(error?.stderr ?? "").trim().split("\n").at(-1).trim().slice(0, 160) : "";
  const code = Number.isInteger(error?.code) ? `exit ${error.code}` : error instanceof SyntaxError ? "unreadable output" : "could not run";
  return line ? `${code}: ${line}` : code;
}

// Shared by the Mac/phone status check and the ACP bridge's model discovery.
export async function installedOpenCode(binary, { signal } = {}) {
  if (!binary) return { installed: false, version: null };
  const version = await run(binary, ["--version"], 10_000, signal)
    .then(text => text.trim().split("\n").at(-1) || null).catch(() => { signal?.throwIfAborted(); return null; });
  return { installed: version != null, version };
}

// OpenCode 2 lists its models through the server API: `opencode api model.list`
// prints `{ location, data: [model] }` with each model's name, cost tiers,
// reasoning variants and context limit. (`opencode models` prints only ids.)
// A model is `providerID/id`; `id` can differ from `modelID` for variants such
// as `-fast`, and only `id` selects the model.
export function parseModels(output) {
  const rows = JSON.parse(output)?.data;
  if (!Array.isArray(rows)) throw new SyntaxError("model list has no data");
  return rows.filter(info => typeof info?.providerID === "string" && typeof info.id === "string").map(info => ({
    id: `${info.providerID}/${info.id}`, name: typeof info.name === "string" ? info.name : `${info.providerID}/${info.id}`, providerId: info.providerID,
    free: Array.isArray(info.cost) && info.cost.length > 0 && info.cost.every(tier => tier?.input === 0 && tier?.output === 0),
    reasoning: Array.isArray(info.variants) && info.variants.length > 0,
    context: Number.isFinite(info.limit?.context) ? info.limit.context : null,
    status: info.enabled === false ? "disabled" : info.status ?? "active",
  }));
}

// `opencode debug config` prints the configuration documents in the order
// OpenCode applies them (global, then the project's); the last one that sets
// `model` wins. It writes `model` as `{ providerID, model }`, and a plain
// `provider/model` string is accepted as well. Nothing else is read.
export function configuredModel(output) {
  const documents = JSON.parse(output);
  if (!Array.isArray(documents)) throw new SyntaxError("configuration is not a document list");
  let chosen = null;
  for (const { info } of documents) {
    const value = info?.model;
    if (typeof value === "string" && value) chosen = value;
    else if (typeof value?.providerID === "string" && typeof value.model === "string") chosen = `${value.providerID}/${value.model}`;
  }
  return chosen;
}

const PROVIDERS = { opencode: "OpenCode Zen", lmstudio: "LM Studio", anthropic: "Anthropic", openai: "OpenAI", google: "Google", openrouter: "OpenRouter" };
export const providerName = id => PROVIDERS[id] ?? id;

// Each command runs only after the previous one exits, so no two of them open
// OpenCode's database together. A failed read is reported as failed, never as
// "no models" or "no default": the bridge then keeps what the last good check
// found. A missing CLI is the only "not installed".
export async function inspectOpenCode(binary, { signal } = {}) {
  if (!binary) return { installed: false, version: null, models: [], missing: true };
  const installation = await installedOpenCode(binary, { signal });
  if (!installation.installed) return { ...installation, models: [], modelsError: "version check failed" };
  const listing = await runOpenCode(binary, ["api", "model.list"], 10_000, signal)
    .then(text => ({ models: parseModels(text).filter(m => m.status !== "deprecated" && m.status !== "disabled") }))
    .catch(error => { signal?.throwIfAborted(); return { models: [], modelsError: probeFailure(error) }; });
  // Let OpenCode resolve JSONC, includes and its configuration precedence.
  // The remaining configuration may contain secrets; never retain or log it.
  const config = await runOpenCode(binary, ["debug", "config"], 10_000, signal)
    .then(text => ({ defaultModel: configuredModel(text) })).catch(error => { signal?.throwIfAborted(); return { defaultModel: null, configError: probeFailure(error, { stderr: false }) }; });
  return { ...installation, ...config, ...listing };
}

// Provider policy is separate from the shared ACP lifecycle. Native model IDs
// are opaque: Cursor's ACP IDs include parameter brackets, unlike its CLI aliases.
export const openCodeAdapter = withPolicy({
  family: "opencode", label: "OpenCode", permissionKey: "opencodePermission",
  workModes: { default: "build", plan: "plan" },
  nativeModel, validatePromptComposition, providerName,
});
export const cursorAdapter = withPolicy({
  family: "cursor", label: "Cursor", permissionKey: "acpPermission",
  workModes: { default: "agent", plan: "plan" },
  nativeModel(value) {
    if (typeof value !== "string" || !value.startsWith("cursor:") || value.length <= 7 || value.length > 1024 || /[\x00-\x20\x7f]/.test(value))
      throw new Error("Choose a Cursor model for this conversation.");
    return value.slice(7);
  },
  // No OpenCode-specific attachment/command restriction is applied to Cursor.
  validatePromptComposition() {},
  providerName: () => "Cursor",
  async authenticate(agent) {
    if (!agent.info?.authMethods?.some(method => method.id === "cursor_login"))
      throw new Error("This Cursor version does not offer the expected sign-in method. Update Cursor on your computer.");
    // cursor_login reads existing credentials; it does not launch a login UI.
    await agent.request("authenticate", { methodId: "cursor_login" }, { timeoutMs: 10_000 });
  },
});

export async function locateCursor(env = process.env, platform = process.platform) {
  const candidates = env.WONDER_CURSOR_BIN ? [env.WONDER_CURSOR_BIN]
    : [...(env.PATH ?? "").split(delimiter).filter(Boolean), ...(platform === "win32" ? [] : SEARCH)]
      .map(dir => join(dir, platform === "win32" ? "agent.exe" : "agent"));
  for (const path of candidates) {
    try { await access(path, constants.X_OK); return path; } catch {}
  }
  return null;
}

export async function installedCursor(binary, { signal } = {}) {
  if (!binary) return { installed: false, version: null, connected: false, missing: true };
  const [version, status] = await Promise.all([
    run(binary, ["--version"], 10_000, signal).then(text => text.trim().split("\n").at(-1) || null).catch(() => null),
    run(binary, ["status", "--format", "json"], 10_000, signal).then(text => JSON.parse(text)).catch(() => null),
  ]);
  // A status check that failed has not shown that Cursor is signed out.
  return { installed: version != null, version, connected: status?.isAuthenticated === true, ...(status == null ? { statusFailed: true } : {}) };
}

export function cursorModels(state) {
  const option = state?.configOptions?.find(option => option.category === "model" && option.type === "select");
  const rows = option ? option.options?.flatMap(row => row.options ?? [row]).map(row => ({ id: row.value, name: row.name }))
    : state?.models?.availableModels?.map(row => ({ id: row.modelId, name: row.name }));
  const models = (rows ?? []).filter(row => typeof row.id === "string" && row.id && row.id.length <= 1017 && !/[\x00-\x20\x7f]/.test(row.id))
    // The daemon names models for every provider (crates/wonderd/src/model_names.rs):
    // Cursor's slug names ("gpt-5.6-sol") pass through as Cursor reports them.
    .map(row => ({ id: row.id, name: typeof row.name === "string" && row.name.trim() ? row.name : row.id, providerId: "cursor" }));
  return { models, defaultModel: option?.currentValue ?? state?.models?.currentModelId ?? null };
}

// Files whose change can change what a check finds: where the CLI's own
// configuration and sign-in live. Absent files count too, so installing or
// signing in is noticed. A remote account or model change touches none of
// these; the bridge's bounded freshness covers that.
export function settingsFiles(family, env = process.env, home = homedir()) {
  if (family === "cursor") return [join(home, ".cursor", "cli-config.json"), join(env.XDG_CONFIG_HOME || join(home, ".config"), "cursor", "auth.json")];
  const config = join(env.XDG_CONFIG_HOME || join(home, ".config"), "opencode");
  return [join(config, "opencode.json"), join(config, "opencode.jsonc"), join(config, "config.json"),
    join(env.XDG_DATA_HOME || join(home, ".local", "share"), "opencode", "auth.json"), ...(env.OPENCODE_CONFIG ? [env.OPENCODE_CONFIG] : [])];
}

// A cheap identity of what a check would read: the located CLI (its resolved
// path, size and time, so an update or a move shows) and the settings files.
// Only metadata; no file is read.
export async function setupFingerprint(binary, files) {
  const entry = async path => {
    const info = await stat(path).catch(() => null);
    return info ? [path, info.size, info.mtimeMs] : [path, null];
  };
  const resolved = binary ? await realpath(binary).catch(() => binary) : null;
  return JSON.stringify([resolved && await entry(resolved), ...await Promise.all(files.map(entry))]);
}

const SESSION_FOLDER =/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

// Cursor 2026.10.16 persists every ACP session, empty ones included, under
// ~/.cursor/acp-sessions/<id>/meta.json, and has no session/close. A model
// check's session is the only one Cupola starts in the runtime's own folder
// (conversations run in their Project folder), so a recorded cwd equal to
// `root` identifies Cupola's probes exactly; every other session is left alone.
export async function removeProbeSessions(root, cursorHome = join(homedir(), ".cursor")) {
  const folder = join(cursorHome, "acp-sessions");
  const own = new Set([resolve(root), await realpath(root).catch(() => resolve(root))]);
  let removed = 0;
  for (const name of await readdir(folder).catch(() => [])) {
    if (!SESSION_FOLDER.test(name)) continue;
    const path = join(folder, name);
    if (!(await lstat(path).catch(() => null))?.isDirectory()) continue;
    const meta = await readFile(join(path, "meta.json"), "utf8").then(JSON.parse).catch(() => null);
    if (typeof meta?.cwd !== "string" || !own.has(resolve(meta.cwd))) continue;
    await rm(path, { recursive: true, force: true }).then(() => { removed++; }, () => {});
  }
  return removed;
}

// ACP is authoritative for model IDs. `agent models` prints CLI aliases that
// cannot be substituted for ACP configuration choices. Discover in a
// short-lived session, never prompt, then end that process group (Cursor's
// worker-server outlives `agent acp` otherwise) and remove the probe sessions
// Cursor kept. The bridge caches this result (see AcpBridge.catalog).
export async function inspectCursor(binary, root, { signal, cursorHome } = {}) {
  signal?.throwIfAborted();
  const installation = await installedCursor(binary, { signal });
  signal?.throwIfAborted();
  if (!installation.connected) return { ...installation, models: [] };
  const { AcpAgent } = await import("./agent.mjs");
  const agent = new AcpAgent({ command: binary, args: ["acp"], cwd: root, label: "Cursor", processGroup: true,
    onPermission: () => ({ outcome: "cancelled" }), onUpdate: () => {} });
  const abort = () => { void agent.close(); };
  signal?.addEventListener("abort", abort, { once: true });
  try {
    signal?.throwIfAborted();
    await agent.start({ timeoutMs: 5000 });
    await cursorAdapter.authenticate(agent);
    const state = await agent.request("session/new", { cwd: root, mcpServers: [] }, { timeoutMs: 10_000 });
    return { ...installation, ...cursorModels(state) };
  } finally {
    signal?.removeEventListener("abort", abort);
    await agent.close();
    await removeProbeSessions(root, cursorHome);
  }
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const action = process.argv[2];
  if (action === "cursor-status") {
    const binary = await locateCursor();
    const status = await installedCursor(binary);
    process.stdout.write(`${status.version ?? ""}\t${status.connected ? "Cursor account" : ""}\n`);
    process.exitCode = !binary ? 44 : !status.installed || status.statusFailed ? 1 : status.connected ? 0 : 42;
  } else if (action === "status") {
    const binary = await locateOpenCode();
    const status = await installedOpenCode(binary);
    process.stdout.write(`${status.version ?? ""}\t\n`);
    process.exitCode = status.installed ? 46 : binary ? 1 : 44;
  } else throw new Error("Expected ACP provider status action.");
}

import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { mkdir, mkdtemp, readFile, realpath, rm, writeFile } from "node:fs/promises";
import { execFile } from "node:child_process";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { promisify } from "node:util";
import { HAIKU_MODEL, usageWindow } from "./projection.mjs";

const execute = promisify(execFile);

// Windows environment names are case-insensitive and usually arrive as Path,
// SystemRoot and ComSpec. Both bridge launch and SDK subprocesses need them.
const HOST_ENV = new Set(["HOME", "PATH", "USER", "LOGNAME", "SHELL", "TMPDIR", "LANG", "LC_ALL",
  "USERPROFILE", "HOMEDRIVE", "HOMEPATH", "APPDATA", "LOCALAPPDATA", "SYSTEMROOT", "WINDIR",
  "COMSPEC", "PATHEXT", "TEMP", "TMP", "USERNAME", "SYSTEMDRIVE", "PROGRAMDATA", "ALLUSERSPROFILE",
  "PROGRAMFILES", "PROGRAMFILES(X86)", "PROGRAMW6432", "COMMONPROGRAMFILES", "COMMONPROGRAMFILES(X86)",
  "COMMONPROGRAMW6432", "USERDOMAIN", "COMPUTERNAME", "OS", "PROCESSOR_ARCHITECTURE"]);
const hostEnvironment = source => Object.fromEntries(Object.entries(source).filter(([name]) => HOST_ENV.has(name.toUpperCase())));

export async function loadSdk(packageRoot) {
  const require = createRequire(packageRoot ? join(packageRoot, "package.json") : import.meta.url);
  const entry = require.resolve("@anthropic-ai/claude-agent-sdk");
  const metadata = JSON.parse(await readFile(join(dirname(entry), "package.json"), "utf8"));
  const sdk = await import(pathToFileURL(entry).href);
  for (const name of ["query", "tool", "createSdkMcpServer"]) {
    if (typeof sdk[name] !== "function") throw new Error(`Claude SDK is missing ${name}`);
  }
  // The owner's Program setting (Settings > Claude > Program) replaces the bundled CLI.
  const executable = process.env.WONDER_CLAUDE_BIN
    || require.resolve(`@anthropic-ai/claude-agent-sdk-${process.platform}-${process.arch}/claude${process.platform === "win32" ? ".exe" : ""}`);
  return { sdk, executable, entry, version: metadata.version };
}

// Names the owner chose to pass on (Settings > provider > Environment). The host
// vets them (provider_environment.rs); this refuses the ones that would change
// how the program itself loads.
const FORBIDDEN_PREFIXES = ["WONDER_", "CUPOLA_", "LD_", "DYLD_", "NODE_", "PYTHON", "RUBY", "BASH", "LC_"];
export function selectedNames(source = process.env) {
  return String(source.CUPOLA_PROVIDER_ENV ?? "").split(",").map(name => name.trim())
    .filter(name => /^[A-Za-z_][A-Za-z0-9_]*$/.test(name) && !HOST_ENV.has(name.toUpperCase())
      && !FORBIDDEN_PREFIXES.some(prefix => name.toUpperCase().startsWith(prefix)));
}

// Preserve the CLI's own access choice, while excluding executable injection
// and unrelated host credentials. Service launches supply an explicit selection.
export function providerEnvironment(source = process.env) {
  const env = hostEnvironment(source);
  const names = [
    "CLAUDE_CONFIG_DIR", "ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_BASE_URL",
    "CLAUDE_CODE_OAUTH_TOKEN", "CLAUDE_CODE_GIT_BASH_PATH", "CLAUDE_CODE_USE_POWERSHELL_TOOL", "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY",
    "http_proxy", "https_proxy", "all_proxy", "no_proxy", "SSL_CERT_FILE", "SSL_CERT_DIR", "NODE_EXTRA_CA_CERTS"];
  for (const name of [...names, ...selectedNames(source)]) if (source[name] !== undefined) env[name] = source[name];
  if (env.CLAUDE_CONFIG_DIR) env.CLAUDE_CONFIG_DIR = resolve(env.CLAUDE_CONFIG_DIR);
  return { ...env, DISABLE_TELEMETRY: "1", DISABLE_ERROR_REPORTING: "1", DISABLE_AUTOUPDATER: "1",
    CLAUDE_AGENT_SDK_CLIENT_APP: "wonder/1.0" };
}

// SDK installation/history checks do not need provider credentials.
export function maintenanceEnvironment(source = process.env) {
  return providerEnvironment(hostEnvironment(source));
}

// Read only user env settings: enabling settingSources would also enable user
// hooks and MCP tools outside Cupola's tool policy. User env settings override
// shell access settings, matching the native CLI; host identity stays fixed.
export function claudeEnvironment(source = process.env) {
  let settings = {};
  const home = source.HOME || source.USERPROFILE;
  const folder = source.CLAUDE_CONFIG_DIR || (home ? join(home, ".claude") : null);
  if (folder) {
    try { settings = JSON.parse(readFileSync(join(folder, "settings.json"), "utf8")).env ?? {}; }
    catch (error) { if (error.code !== "ENOENT") throw new Error("Claude user settings could not be read"); }
  }
  const values = Object.fromEntries(Object.entries(settings).filter(([name, value]) => typeof value === "string"
    && !HOST_ENV.has(name.toUpperCase()) && name.toUpperCase() !== "CLAUDE_CONFIG_DIR"));
  return providerEnvironment({ ...source, ...values });
}

export function accessKind(account, env = process.env) {
  if ((account?.apiKeySource && account.apiKeySource !== "none") || account?.tokenSource === "ANTHROPIC_AUTH_TOKEN") return "API key";
  if (isSubscription(account) || (account?.tokenSource && account.tokenSource !== "none" && env.CLAUDE_CODE_OAUTH_TOKEN)) return "Claude subscription";
  return null;
}

export function isSubscription(account) {
  return account?.apiProvider === "firstParty"
    && (account.apiKeySource == null || account.apiKeySource === "none")
    && (account.tokenSource === "claude.ai"
      // Any named plan counts: Anthropic adds tiers (Claude Ultra) without a runtime release.
      || (account.tokenSource == null && typeof account.subscriptionType === "string" && account.subscriptionType.trim() !== ""));
}

// `claude auth status` reads the stored login without starting a model or a session.
// Report the native access method without starting a model.
export function classifyAuthStatus(status, env = process.env) {
  if (!status || typeof status !== "object" || typeof status.loggedIn !== "boolean") return { exitCode: 1, account: "" };
  if (!status.loggedIn) return { exitCode: 42, account: "" };
  const plan = typeof status.subscriptionType === "string" ? status.subscriptionType : "";
  const account = plan ? `Claude ${plan.charAt(0).toUpperCase()}${plan.slice(1)}` : "";
  if (status.authMethod === "api_key" || (status.authMethod === "oauth_token" && env.ANTHROPIC_AUTH_TOKEN) || status.apiKeySource && status.apiKeySource !== "none")
    return { exitCode: 0, account: "API key" };
  if (status.authMethod === "oauth_token" && env.CLAUDE_CODE_OAUTH_TOKEN) return { exitCode: 0, account: "Claude subscription" };
  const subscription = isSubscription({ apiProvider: status.apiProvider, subscriptionType: plan,
    tokenSource: status.authMethod === "claude.ai" ? "claude.ai" : status.authMethod ?? "none" });
  return subscription ? { exitCode: 0, account: account || "Claude subscription" } : { exitCode: 45, account };
}

// Launch arguments from the owner (a JSON array in WONDER_CLAUDE_ARGS) as the SDK's
// `extraArgs`: `--flag value` pairs and bare `--flag`s.
export function extraArguments(source = process.env) {
  let list;
  try { list = JSON.parse(source.WONDER_CLAUDE_ARGS ?? "[]"); } catch { return {}; }
  if (!Array.isArray(list)) return {};
  const result = {};
  for (let i = 0; i < list.length; i++) {
    const item = String(list[i]);
    if (!item.startsWith("--")) continue;
    const [flag, inline] = item.slice(2).split(/=(.*)/s, 2);
    if (inline !== undefined) result[flag] = inline;
    else if (i + 1 < list.length && !String(list[i + 1]).startsWith("--")) result[flag] = String(list[++i]);
    else result[flag] = null;
  }
  return result;
}

export function baseOptions(runtime, { connectors = false } = {}) {
  // Claude in Chrome stays as the owner set it up for Claude Code; its tools
  // ask for approval like other MCP tools (permissions.mjs).
  return { pathToClaudeCodeExecutable: runtime.executable, env: claudeEnvironment(), extraArgs: extraArguments(),
    // Strict MCP also suppresses the account's claude.ai connectors. Empty
    // settingSources still excludes user/project/local MCP servers and hooks.
    model: HAIKU_MODEL, settingSources: [], plugins: [], strictMcpConfig: !connectors,
    settings: { autoMemoryEnabled: false, switchModelsOnFlag: false, autoContinueAtUsageLimit: false,
      disableClaudeAiConnectors: !connectors },
    promptSuggestions: false, agentProgressSummaries: true, stderr: () => {} };
}

export function projectUsage(result) {
  if (result?.rate_limits_available !== true || !result.rate_limits || typeof result.rate_limits !== "object") return null;
  const windows = Object.entries(result.rate_limits).flatMap(([type, value]) => {
    const window = usageWindow(type, value ? { ...value, format: "percent" } : null);
    return window ? [window] : [];
  });
  // Per-model weekly windows the account has (such as "Weekly · Fable").
  for (const scoped of Array.isArray(result.rate_limits.model_scoped) ? result.rate_limits.model_scoped.slice(0, 4) : []) {
    const name = typeof scoped?.display_name === "string" ? scoped.display_name.trim().slice(0, 23) : "";
    const slug = name.toLowerCase().replace(/[^a-z0-9]+/g, "_").replace(/^_|_$/g, "");
    const window = slug ? usageWindow("seven_day", { ...scoped, format: "percent" }) : null;
    if (window && !windows.some(w => w.id === `seven_day_model_${slug}`)) windows.push({ ...window, id: `seven_day_model_${slug}`, label: `Weekly · ${name}` });
  }
  return windows.length ? windows : null;
}

// Usage above the subscription limit is usable only when the account reports
// enabled overage with remaining capacity. Unknown values stay unknown.
export function additionalUsageAvailable(result) {
  const extra = result?.rate_limits?.extra_usage;
  if (result?.rate_limits_available !== true || !extra || typeof extra.is_enabled !== "boolean") return null;
  if (!extra.is_enabled) return false;
  if (typeof extra.utilization === "number" && Number.isFinite(extra.utilization)) return extra.utilization < 100;
  if (extra.monthly_limit === null) return true;
  if (typeof extra.monthly_limit === "number" && typeof extra.used_credits === "number") return extra.used_credits < extra.monthly_limit;
  return null;
}

// The bridge calls these unconditionally to show and fork Claude history.
// listSubagents/getSubagentMessages stay optional: callers check for them.
const HISTORY_READERS = ["listSessions", "getSessionInfo", "getSessionMessages"];
const HISTORY_FUNCTIONS = [...HISTORY_READERS, "forkSession"];
const FIXTURE_SESSION = "0f1e2d3c-4b5a-4968-8778-695a4b3c2d1e";
const FIXTURE_TURNS = [["user", "0f1e2d3c-0000-4000-8000-000000000001", "Fixture question"],
  ["assistant", "0f1e2d3c-0000-4000-8000-000000000002", "Fixture answer"]];
const READ_FIXTURE = `
const [entry, cwd, id] = process.argv.slice(1);
const sdk = await import(entry);
const options = { dir: cwd };
const sessions = await sdk.listSessions({ ...options, includeWorktrees: false, includeProgrammatic: true });
const info = await sdk.getSessionInfo(id, options);
const messages = await sdk.getSessionMessages(id, options);
process.stdout.write(JSON.stringify({ sessions, info, messages }));`;

// Reads a fixed Claude Code transcript with the runtime's own history readers,
// offline and in a child process so its CLAUDE_CONFIG_DIR never touches the
// owner's history. A patch release that changes what these return would leave
// chats empty or unopenable, so it never becomes active.
export async function checkHistoryReaders(runtime) {
  for (const name of HISTORY_FUNCTIONS)
    if (typeof runtime.sdk?.[name] !== "function") throw new Error(`Claude runtime is missing ${name}`);
  const root = await realpath(await mkdtemp(join(tmpdir(), "wonder-claude-history-")));
  try {
    const cwd = join(root, "project"), config = join(root, "config");
    const folder = join(config, "projects", cwd.replace(/[^a-zA-Z0-9]/g, "-"));
    await mkdir(cwd); await mkdir(folder, { recursive: true });
    const timestamp = "2026-01-01T00:00:00.000Z";
    let parentUuid = null;
    const lines = FIXTURE_TURNS.map(([type, uuid, text]) => {
      const message = type === "user" ? { role: "user", content: text }
        : { id: "msg_fixture", type: "message", role: "assistant", model: HAIKU_MODEL, content: [{ type: "text", text }],
          stop_reason: "end_turn", stop_sequence: null, usage: { input_tokens: 1, output_tokens: 1 } };
      const row = { type, uuid, parentUuid, sessionId: FIXTURE_SESSION, cwd, timestamp, isSidechain: false,
        userType: "external", version: "2.0.0", message };
      parentUuid = uuid;
      return JSON.stringify(row);
    });
    await writeFile(join(folder, `${FIXTURE_SESSION}.jsonl`), `${lines.join("\n")}\n`);
    const { stdout } = await execute(process.execPath, ["--input-type=module", "-e", READ_FIXTURE,
      pathToFileURL(runtime.entry).href, cwd, FIXTURE_SESSION], { timeout: 30_000, maxBuffer: 1024 * 1024,
      env: { ...maintenanceEnvironment(), CLAUDE_CONFIG_DIR: config } });
    const { sessions, info, messages } = JSON.parse(stdout);
    const sameSession = row => row?.sessionId === FIXTURE_SESSION && row.cwd === cwd && Number.isFinite(row.lastModified);
    if (!Array.isArray(sessions) || sessions.length !== 1 || !sameSession(sessions[0]))
      throw new Error("Claude runtime lists history differently");
    if (!sameSession(info)) throw new Error("Claude runtime describes history differently");
    const text = content => typeof content === "string" ? content : content?.find?.(part => part?.type === "text")?.text;
    if (!Array.isArray(messages) || messages.length !== FIXTURE_TURNS.length
      || FIXTURE_TURNS.some(([type, uuid, body], i) => messages[i]?.type !== type || messages[i].uuid !== uuid
        || text(messages[i].message?.content) !== body))
      throw new Error("Claude runtime reads history differently");
  } finally {
    await rm(root, { recursive: true, force: true });
  }
}

// These control requests initialize the runtime but never submit a user prompt.
// Keep experimental account data behind this adapter, not in the native protocol.
export async function inspectSdk(runtime, { cwd, includeUsage = false, connectors = false } = {}) {
  const abortController = new AbortController();
  const done = Promise.withResolvers();
  const timer = setTimeout(() => abortController.abort(), 30_000);
  const options = baseOptions(runtime, { connectors });
  const query = runtime.sdk.query({ prompt: (async function* () { await done.promise; })(),
    options: { ...options, cwd, abortController, tools: [], mcpServers: {},
      permissionMode: "dontAsk", permissionPrompts: "none", persistSession: false,
      settings: { ...options.settings, disableAllHooks: true } } });
  try {
    for (const method of ["initializationResult", "accountInfo", "supportedModels", "mcpServerStatus", "interrupt", "close"])
      if (typeof query[method] !== "function") throw new Error(`Claude runtime is missing ${method}`);
    await query.initializationResult();
    const account = await query.accountInfo();
    const models = await query.supportedModels();
    if (!Array.isArray(models) || !models.some(m => typeof m.value === "string" && m.value.includes("haiku")))
      throw new Error("Claude runtime returned an incompatible model catalog");
    const kind = accessKind(account, options.env);
    const connected = kind !== null;
    const usageMethod = query.usage_EXPERIMENTAL_MAY_CHANGE_DO_NOT_RELY_ON_THIS_API_YET;
    let windows = null, additionalUsage = null;
    if (includeUsage && isSubscription(account) && typeof usageMethod === "function") {
      try { const usage = await usageMethod.call(query, { skipBehaviors: true }); windows = projectUsage(usage); additionalUsage = additionalUsageAvailable(usage); } catch { /* Usage is optional; never fake a zero. */ }
    }
    let servers = connectors && connected ? await query.mcpServerStatus() : [];
    // MCP startup is asynchronous, including the account's server list fetch.
    // Give empty/pending discovery up to five seconds before closing the query.
    for (let i = 0; i < 10 && connectors && connected
      && (servers.length === 0 || servers.some(s => s.status === "pending")); i++) {
      await new Promise(resolve => setTimeout(resolve, 500));
      if (abortController.signal.aborted) break;
      servers = await query.mcpServerStatus();
    }
    return { version: runtime.version, connected, accessKind: kind, subscription: isSubscription(account) ? account.subscriptionType : null,
      accountEmail: typeof account.email === "string" ? account.email : null,
      models: models.map(m => ({ id: m.value, resolvedModel: m.resolvedModel, name: m.displayName, description: m.description,
        efforts: m.supportsEffort ? (m.supportedEffortLevels ?? []).filter(e => ["low", "medium", "high", "xhigh", "max"].includes(e)).concat(m.supportedEffortLevels?.includes("max") ? ["ultracode"] : []) : [],
        fast: m.supportsFastMode === true })),
      usageSupported: isSubscription(account) && typeof usageMethod === "function", windows, additionalUsageAvailable: additionalUsage,
      servers: servers.map(s => ({ name: s.name, status: s.status, source: s.source })) };
  } finally {
    clearTimeout(timer); done.resolve(); query.close(); abortController.abort();
  }
}

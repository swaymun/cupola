import test from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, rm, readFile, mkdir, readdir, writeFile, realpath } from "node:fs/promises";
import { tmpdir } from "node:os";
import { basename, join } from "node:path";
import { compatibleVersion, newestPatch, RuntimeUpdates, activeRuntime } from "../runtime-updates.mjs";
import { isSubscription, accessKind, claudeEnvironment, classifyAuthStatus, providerEnvironment, maintenanceEnvironment, projectUsage, additionalUsageAvailable, baseOptions, loadSdk, inspectSdk, checkHistoryReaders } from "../sdk-runtime.mjs";
import { pathToFileURL } from "node:url";

// Contract: new SDK bytes never replace a running turn; a bad candidate and a
// restart must preserve a usable runtime. Exercise the manager's persisted state.
async function fixture(t, overrides = {}) {
  const root = await mkdtemp(join(tmpdir(), "wonder-sdk-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const options = { root, bundled: { version: "0.3.283" }, load: async root => ({ version: basename(root) }),
    check: async () => {}, readers: async () => {}, install: async version => join(root, "versions", version), latest: async () => "0.3.284", ...overrides };
  return { options, manager: await new RuntimeUpdates(options).initialize() };
}
test("activates a checked update only after all active turns finish", async t => {
  const { manager, options } = await fixture(t);
  const activated = [];
  manager.onActivated(runtime => { activated.push(runtime.version); throw new Error("listener failures never block activation"); });
  const first = await manager.acquire(), second = await manager.acquire();
  await manager.refresh();
  assert.equal(manager.status().pendingVersion, "0.3.284");
  assert.equal(manager.current.version, "0.3.283");
  await first.release(); await first.release();
  assert.equal(manager.current.version, "0.3.283");
  assert.deepEqual(activated, []);
  await second.release();
  assert.equal(manager.current.version, "0.3.284");
  assert.equal(first.runtime.version, "0.3.283");
  assert.deepEqual(activated, ["0.3.284"]);
  const restarted = await new RuntimeUpdates(options).initialize();
  assert.equal(restarted.current.version, "0.3.284");
});
// Contract: a patch release whose history readers stop reading a Claude Code
// transcript never becomes current; readers Cupola treats as optional stay so.
test("an update must read an offline transcript before it can activate", async t => {
  const bundled = await loadSdk();
  await checkHistoryReaders(bundled);
  const root = await mkdtemp(join(tmpdir(), "wonder-sdk-readers-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const real = pathToFileURL(bundled.entry).href;
  const required = "query, tool, createSdkMcpServer, getSessionInfo, getSessionMessages, forkSession";
  const variants = {
    "0.3.284": `export * from "${real}"; export async function getSessionMessages() { return []; }`,
    "0.3.285": `export * from "${real}"; export async function listSessions(o) { return []; }`,
    "0.3.286": `export { ${required} } from "${real}";`,
    "0.3.287": `export { ${required}, listSessions } from "${real}";`,
  };
  const runtimes = {};
  for (const [version, source] of Object.entries(variants)) {
    const entry = join(root, `${version}.mjs`);
    await writeFile(entry, source);
    runtimes[version] = { version, entry, sdk: await import(pathToFileURL(entry).href) };
  }
  await assert.rejects(checkHistoryReaders(runtimes["0.3.284"]), /reads history differently/);
  await assert.rejects(checkHistoryReaders(runtimes["0.3.285"]), /lists history differently/);
  await assert.rejects(checkHistoryReaders(runtimes["0.3.286"]), /missing listSessions/);
  // No subagent readers: those features hide themselves instead.
  await checkHistoryReaders(runtimes["0.3.287"]);
  const manager = async latest => {
    const state = join(root, latest);
    return new RuntimeUpdates({ root: state, bundled, load: async path => runtimes[basename(path)], check: async () => {},
      install: async version => join(state, "versions", version), latest: async () => latest }).initialize();
  };
  const broken = await manager("0.3.284");
  await broken.refresh();
  assert.equal(broken.current, bundled);
  assert.equal(broken.status().pendingVersion, null);
  assert.equal(broken.status().lastFailure, "candidate_unavailable_or_incompatible");
  const working = await manager("0.3.287");
  await working.refresh();
  assert.equal(working.current.version, "0.3.287");
});
test("status reads the activated SDK without changing manager state", async t => {
  const { manager, options } = await fixture(t);
  assert.equal((await activeRuntime(options)).version, "0.3.283");
  await manager.refresh();
  const saved = await readFile(join(options.root, "state.json"), "utf8");
  assert.equal((await activeRuntime(options)).version, "0.3.284");
  assert.equal(await readFile(join(options.root, "state.json"), "utf8"), saved);
  assert.equal((await activeRuntime({ ...options, load: async () => { throw new Error("gone"); } })).version, "0.3.283");
  assert.equal((await activeRuntime({ ...options, root: join(options.root, "missing") })).version, "0.3.283");
});
test("a broken candidate cannot displace the bundled version", async t => {
  const { manager } = await fixture(t, { check: async () => { throw new Error("control protocol changed"); } });
  await manager.refresh();
  assert.equal(manager.current.version, "0.3.283");
  assert.equal(manager.status().pendingVersion, null);
  assert.equal(manager.status().lastFailure, "candidate_unavailable_or_incompatible");
});
test("restart falls back when an installed candidate no longer loads", async t => {
  const { manager, options } = await fixture(t);
  await manager.refresh();
  const restarted = await new RuntimeUpdates({ ...options, load: async () => { throw new Error("missing executable"); } }).initialize();
  assert.equal(restarted.current.version, "0.3.283");
  assert.equal(JSON.parse(await readFile(join(options.root, "state.json"))).active, "0.3.283");
});
test("updates are coalesced and breaking/prerelease/path values never install", async t => {
  let checks = 0;
  const { manager } = await fixture(t, { latest: async () => { checks++; return "0.4.0"; }, install: async () => assert.fail("must not install") });
  await Promise.all([manager.refresh(), manager.refresh()]);
  await manager.refresh();
  assert.equal(checks, 1);
  assert.equal(manager.status().lastFailure, null);
  assert.equal(manager.status().adapterUpdateVersion, "0.4.0");
  for (const version of ["../0.3.284", "0.3.284-beta", "0.3.282", "1.0.0", undefined]) assert.equal(compatibleVersion(version, "0.3.283"), false);
});
test("patches of the bundled minor keep arriving after a new minor is latest", async t => {
  const installed = [];
  const { manager } = await fixture(t, {
    releases: async () => ({ latest: "0.4.1", versions: ["0.3.282", "0.3.283", "0.3.290", "0.4.0", "0.4.1", "0.3.291", "0.3.292-beta"] }),
    install: async version => { installed.push(version); return join(manager.root, "versions", version); } });
  await manager.refresh();
  assert.deepEqual(installed, ["0.3.291"]);
  assert.equal(manager.current.version, "0.3.291");
  assert.deepEqual({ ...manager.status(), checkedAtMs: null }, { activeVersion: "0.3.291", bundledVersion: "0.3.283",
    pendingVersion: null, checkedAtMs: null, checking: false, lastFailure: null, latestVersion: "0.4.1",
    newestCompatibleVersion: "0.3.291", adapterUpdateVersion: "0.4.1" });
  assert.equal(newestPatch(["0.4.0", "1.0.0"], "0.3.283"), null);
});
test("the registry check reads every published version and skips deprecated ones", async t => {
  const { manager } = await fixture(t, { latest: undefined, releases: undefined });
  const fetched = [];
  t.mock.method(globalThis, "fetch", async (url, init) => {
    fetched.push([url, init.headers.accept]);
    return new Response(JSON.stringify({ "dist-tags": { latest: "0.4.0" },
      versions: { "0.3.283": {}, "0.3.300": { deprecated: "broken build" }, "0.3.299": {}, "0.4.0": {} } }));
  });
  assert.deepEqual(await manager.registryReleases(), { latest: "0.4.0", versions: ["0.3.283", "0.3.299", "0.4.0"] });
  assert.deepEqual(fetched, [["https://registry.npmjs.org/@anthropic-ai%2Fclaude-agent-sdk", "application/vnd.npm.install-v1+json"]]);
});
test("access classification distinguishes subscriptions and explicit API keys", () => {
  const pro = { apiProvider: "firstParty", subscriptionType: "Claude Pro", apiKeySource: "none" };
  assert.equal(isSubscription(pro), true);
  assert.equal(accessKind(pro, {}), "Claude subscription");
  assert.equal(accessKind({ apiProvider: "firstParty", tokenSource: "claude.ai" }, {}), "Claude subscription");
  assert.equal(classifyAuthStatus({ loggedIn: true, authMethod: "claude.ai", apiProvider: "firstParty" }, {}).exitCode, 0);
  assert.equal(accessKind({ apiProvider: "firstParty", apiKeySource: "ANTHROPIC_API_KEY" }, {}), "API key");
  assert.equal(accessKind({ apiProvider: "firstParty", tokenSource: "ANTHROPIC_AUTH_TOKEN" }, {}), "API key");
  assert.equal(accessKind({ apiProvider: "firstParty", tokenSource: "env" }, { CLAUDE_CODE_OAUTH_TOKEN: "fixture" }), "Claude subscription");
  assert.equal(classifyAuthStatus({ loggedIn: true, authMethod: "oauth_token" }, { CLAUDE_CODE_OAUTH_TOKEN: "fixture" }).account, "Claude subscription");
  // A plan the runtime has never heard of is still a subscription (Claude Code 0.3.312 reports "Claude Ultra"); a signed-out account names none.
  assert.equal(accessKind({ apiProvider: "firstParty", subscriptionType: "Claude Ultra" }, {}), "Claude subscription");
  assert.equal(accessKind({ apiProvider: "firstParty", subscriptionType: null }, {}), null);
  assert.equal(isSubscription({ ...pro, apiKeySource: "ANTHROPIC_API_KEY" }), false);
  assert.equal(isSubscription({ ...pro, apiProvider: "bedrock" }), false);
  assert.deepEqual(providerEnvironment({ HOME: "/home/test", ANTHROPIC_API_KEY: "secret", NODE_OPTIONS: "--require evil", CODEX_HOME: "private" }),
    { HOME: "/home/test", ANTHROPIC_API_KEY: "secret", DISABLE_TELEMETRY: "1", DISABLE_ERROR_REPORTING: "1", DISABLE_AUTOUPDATER: "1", CLAUDE_AGENT_SDK_CLIENT_APP: "wonder/1.0" });
  // The configured native folder reaches Claude Code.
  assert.equal(providerEnvironment({ HOME: "/home/test", CLAUDE_CONFIG_DIR: "/data/account" }).CLAUDE_CONFIG_DIR, "/data/account");
  // Settings status applies the same rule to `claude auth status --json`.
  const signedIn = { loggedIn: true, authMethod: "claude.ai", apiProvider: "firstParty", subscriptionType: "max", email: "private@example.com" };
  assert.deepEqual(classifyAuthStatus(signedIn), { exitCode: 0, account: "Claude Max" });
  assert.deepEqual(classifyAuthStatus({ ...signedIn, authMethod: "api_key" }), { exitCode: 0, account: "API key" });
  assert.deepEqual(classifyAuthStatus({ ...signedIn, apiProvider: "bedrock" }).exitCode, 45);
  assert.deepEqual(classifyAuthStatus({ loggedIn: false, authMethod: "none" }), { exitCode: 42, account: "" });
  assert.deepEqual(classifyAuthStatus({}), { exitCode: 1, account: "" });
});
test("Windows child environment preserves lookup and profile paths without executable injection", () => {
  const host = { Path: String.raw`C:\Program Files\nodejs;C:\Users\Test User\AppData\Roaming\npm`,
    PATHEXT: ".COM;.EXE;.BAT;.CMD", SystemRoot: String.raw`C:\Windows`, ComSpec: String.raw`C:\Windows\System32\cmd.exe`,
    USERPROFILE: String.raw`C:\Users\Test User`, APPDATA: String.raw`C:\Users\Test User\AppData\Roaming`,
    LOCALAPPDATA: String.raw`C:\Users\Test User\AppData\Local`, TEMP: String.raw`C:\Users\Test User\AppData\Local\Temp` };
  const source = { ...host, NODE_OPTIONS: "--require evil", WONDER_LOOPBACK_CAPABILITY: "private", UNRELATED_API_KEY: "private",
    ANTHROPIC_API_KEY: "provider-key", CLAUDE_CODE_GIT_BASH_PATH: String.raw`D:\Git for Windows\bin\bash.exe`, CLAUDE_CODE_USE_POWERSHELL_TOOL: "1" };
  for (const environment of [providerEnvironment(source), maintenanceEnvironment(source)]) {
    for (const [name, value] of Object.entries(host)) assert.equal(environment[name], value, name);
    for (const name of ["NODE_OPTIONS", "WONDER_LOOPBACK_CAPABILITY", "UNRELATED_API_KEY"]) assert.equal(environment[name], undefined);
  }
  assert.equal(providerEnvironment(source).ANTHROPIC_API_KEY, "provider-key");
  assert.equal(maintenanceEnvironment(source).ANTHROPIC_API_KEY, undefined);
  assert.equal(providerEnvironment(source).CLAUDE_CODE_GIT_BASH_PATH, source.CLAUDE_CODE_GIT_BASH_PATH);
  assert.equal(providerEnvironment(source).CLAUDE_CODE_USE_POWERSHELL_TOOL, "1");
  assert.equal(maintenanceEnvironment(source).CLAUDE_CODE_GIT_BASH_PATH, undefined);
});

test("experimental usage is optional and percentages are never fractions", () => {
  assert.equal(projectUsage({ rate_limits_available: false }), null);
  assert.equal(projectUsage({ rate_limits_available: true, rate_limits: { future: { utilization: 10 } } }), null);
  const result = projectUsage({ rate_limits_available: true, rate_limits: { five_hour: { utilization: 10 }, seven_day: { utilization: 20 } } });
  assert.deepEqual(result.map(w => [w.id, w.remainingPercent]), [["five_hour", 90], ["seven_day", 80]]);
  // Per-model weekly windows keep the name Claude gives them.
  const scoped = projectUsage({ rate_limits_available: true, rate_limits: { seven_day: { utilization: 2 },
    model_scoped: [{ display_name: "Fable", utilization: 0, resets_at: "2026-10-18T17:00:00Z" }, { display_name: "", utilization: 5 }] } });
  assert.deepEqual(scoped.map(w => [w.id, w.label, w.usedPercent, w.windowDurationMins]),
    [["seven_day", "Weekly limit", 2, 10080], ["seven_day_model_fable", "Weekly · Fable", 0, 10080]]);
});

test("overage availability remains unknown unless reported and usable", () => {
  const usage = extra_usage => ({ rate_limits_available: true, rate_limits: { extra_usage } });
  assert.equal(additionalUsageAvailable({}), null);
  assert.equal(additionalUsageAvailable(usage({ is_enabled: false, credits_ever_enabled: true })), false);
  assert.equal(additionalUsageAvailable(usage({ is_enabled: true, utilization: 99 })), true);
  assert.equal(additionalUsageAvailable(usage({ is_enabled: true, utilization: 100 })), false);
  assert.equal(additionalUsageAvailable(usage({ is_enabled: true, monthly_limit: null })), true);
  assert.equal(additionalUsageAvailable(usage({ is_enabled: true, monthly_limit: 100, used_credits: 100 })), false);
  assert.equal(additionalUsageAvailable(usage({ is_enabled: true })), null);
});

test("control discovery waits for pending connections without submitting a prompt", async () => {
  let calls = 0, closed = false, input, nextInput;
  const runtime = { version: "0.3.283", sdk: { query: ({ prompt }) => {
    input = prompt[Symbol.asyncIterator](); nextInput = input.next();
    return {
    initializationResult: async () => ({}),
    accountInfo: async () => ({ apiProvider: "firstParty", subscriptionType: "Claude Pro" }),
    supportedModels: async () => [{ value: "haiku", resolvedModel: "claude-haiku-4-5-20251001" }],
    mcpServerStatus: async () => {
      assert.equal(closed, false);
      if (++calls === 1) return [];
      return [{ name: "Mail", source: "claudeai", status: calls === 2 ? "pending" : "connected" }];
    },
    interrupt: async () => {}, close: () => { closed = true; },
  }; } } };
  const result = await inspectSdk(runtime, { connectors: true });
  assert.equal(result.servers[0].status, "connected");
  assert.equal(result.models[0].resolvedModel, "claude-haiku-4-5-20251001");
  assert.equal(calls, 3);
  assert.equal(closed, true);
  assert.deepEqual(await nextInput, { done: true, value: undefined });
});

test("a staged update survives restart and old versions are bounded", async t => {
  let latest = "0.3.284";
  const { manager, options } = await fixture(t, { latest: async () => latest });
  for (const version of ["0.3.281", "0.3.284", "notes"]) await mkdir(join(options.root, "versions", version));
  const running = await manager.acquire();
  await manager.refresh();
  assert.equal(manager.current.version, "0.3.283");
  const restarted = await new RuntimeUpdates(options).initialize();
  assert.equal(restarted.current.version, "0.3.284");
  assert.deepEqual((await readdir(join(options.root, "versions"))).sort(), ["0.3.284", "notes"]);
  latest = "0.3.285";
  await mkdir(join(options.root, "versions", latest));
  await restarted.refresh({ force: true });
  assert.equal(restarted.current.version, "0.3.285");
  assert.equal(restarted.state.lastGood, "0.3.284");
  assert.deepEqual((await readdir(join(options.root, "versions"))).sort(), ["0.3.284", "0.3.285", "notes"]);
  // This lease represents the process that died; do not activate it again.
  assert.equal(running.runtime.version, "0.3.283");
});

test("finishing a turn during the staged-state commit cannot race activation", async t => {
  const { manager, options } = await fixture(t);
  const running = await manager.acquire();
  const entered = Promise.withResolvers(), proceed = Promise.withResolvers();
  const save = manager.save.bind(manager);
  let first = true;
  manager.save = async () => { if (first) { first = false; entered.resolve(); await proceed.promise; } await save(); };
  const refreshing = manager.refresh();
  await entered.promise;
  await running.release();
  assert.equal(manager.current.version, "0.3.283");
  proceed.resolve(); await refreshing;
  assert.equal(manager.current.version, "0.3.284");
  assert.equal(JSON.parse(await readFile(join(options.root, "state.json"))).active, "0.3.284");
});

test("shutdown aborts discovery and drains the owner without activating an update", async t => {
  const { manager, options } = await fixture(t);
  const entered = Promise.withResolvers();
  manager.latest = () => new Promise((_, reject) => {
    manager.abort.signal.addEventListener("abort", () => reject(new Error("closed")), { once: true });
    entered.resolve();
  });
  const refreshing = manager.refresh();
  await entered.promise;
  await manager.close();
  await refreshing;
  assert.equal(manager.current.version, "0.3.283");
  assert.equal(manager.refreshing, null);
  assert.equal(JSON.parse(await readFile(join(options.root, "state.json"))).active, "0.3.283");
  manager.latest = () => assert.fail("closed managers cannot restart discovery");
  await manager.refresh({ force: true });
});

// Contract: enabling account connectors must not execute MCP commands from the
// user's CLI config or the Project's workspace. Exercise the actual pinned SDK with
// an isolated, unauthenticated HOME; no model prompt or network credential.
test("account connector options exclude user, project and local MCP processes", async t => {
  const root = await realpath(await mkdtemp(join(tmpdir(), "wonder-mcp-scope-")));
  // Windows keeps the workspace busy until the closed CLI and its servers exit.
  t.after(() => rm(root, { recursive: true, force: true, maxRetries: 20, retryDelay: 250 }));
  const home = join(root, "home"), cwd = join(root, "workspace"), config = join(home, ".claude");
  await mkdir(config, { recursive: true }); await mkdir(cwd);
  const server = join(root, "fixture.mjs"), marker = join(root, "started");
  await writeFile(server, `import { appendFileSync } from 'node:fs';
import { createInterface } from 'node:readline';
appendFileSync(process.argv[2], process.argv[3] + '\\n');
for await (const line of createInterface({ input: process.stdin })) {
  const request = JSON.parse(line);
  if (request.id == null) continue;
  const result = request.method === 'initialize'
    ? { protocolVersion: request.params.protocolVersion, capabilities: { tools: {} }, serverInfo: { name: 'fixture', version: '1' } }
    : { tools: [] };
  process.stdout.write(JSON.stringify({ jsonrpc: '2.0', id: request.id, result }) + '\\n');
}
`);
  const command = scope => ({ command: process.execPath, args: [server, marker, scope] });
  await writeFile(join(config, ".claude.json"), JSON.stringify({
    mcpServers: { fixture_user: command("user") },
    // Claude Code keys projects by path with forward slashes (C:/Users/... on Windows).
    projects: { [cwd.replaceAll("\\", "/")]: { mcpServers: { fixture_local: command("local") } } },
  }));
  await writeFile(join(cwd, ".mcp.json"), JSON.stringify({ mcpServers: { fixture_project: command("project") } }));
  const runtime = await loadSdk();
  const inspect = async overrides => {
    const base = baseOptions(runtime, { connectors: true });
    const abortController = new AbortController(), done = Promise.withResolvers();
    const timer = setTimeout(() => abortController.abort(), 15_000);
    const query = runtime.sdk.query({ prompt: (async function* () { await done.promise; })(), options: {
      ...base, ...overrides, cwd, abortController, tools: [], mcpServers: {}, persistSession: false,
      env: { ...base.env, HOME: home, CLAUDE_CONFIG_DIR: config }, permissionMode: "dontAsk",
    } });
    try {
      await query.initializationResult();
      let servers = await query.mcpServerStatus();
      for (let i = 0; i < 50 && servers.some(s => s.status === "pending"); i++) {
        await new Promise(resolve => setTimeout(resolve, 100));
        servers = await query.mcpServerStatus();
      }
      return servers;
    } finally { clearTimeout(timer); done.resolve(); query.close(); abortController.abort(); }
  };
  assert.deepEqual(await inspect({}), []);
  await assert.rejects(readFile(marker), { code: "ENOENT" });
  // Positive control proves all three fixture configs are readable by this SDK.
  const enabled = await inspect({ settingSources: ["user", "project", "local"] });
  assert.deepEqual(enabled.map(s => s.name).sort(), ["fixture_local", "fixture_project", "fixture_user"]);
  assert.deepEqual((await readFile(marker, "utf8")).trim().split("\n").sort(), ["local", "project", "user"]);
});

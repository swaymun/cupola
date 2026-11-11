import { test } from "node:test";
import assert from "node:assert/strict";
import { chmod, mkdir, mkdtemp, readFile, readdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { execFile } from "node:child_process";
import { createRequire } from "node:module";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";
import { AcpAgent } from "../agent.mjs";
import { AcpBridge } from "../bridge.mjs";
import { Sessions } from "../sessions.mjs";
import { inspectOpenCode, installedOpenCode, parseModels, configuredModel, locateOpenCode, openCodeCommand, inspectCursor, installedCursor, locateCursor, cursorModels, cursorAdapter, openCodeAdapter, settingsFiles, setupFingerprint } from "../catalog.mjs";

const fixture = name => readFile(new URL(`./fixtures/opencode-2.2.5/${name}`, import.meta.url), "utf8");

test("discovery reads OpenCode 2's model API and resolved configuration and keeps no credentials", async t => {
  const root = await mkdtemp(join(tmpdir(), "wonder-opencode-catalog-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const binary = join(root, "opencode");
  // Real 2.2.5 outputs, plus one deprecated and one disabled model the API can report.
  const list = JSON.parse(await fixture("api-model-list.json"));
  list.data.push({ ...list.data[0], id: "retired", modelID: "retired", status: "deprecated" }, { ...list.data[0], id: "off", modelID: "off", enabled: false });
  await writeFile(join(root, "list.json"), JSON.stringify(list));
  await writeFile(join(root, "config.json"), await fixture("debug-config.json"));
  await writeFile(binary, `#!${process.execPath}
const { readFileSync } = require("node:fs");
const args = process.argv.slice(2).join(" ");
if (args === "--version") console.log("2.2.5");
else if (args === "api model.list") process.stdout.write(readFileSync(${JSON.stringify(join(root, "list.json"))}));
else if (args === "debug config") process.stdout.write(readFileSync(${JSON.stringify(join(root, "config.json"))}));
else { console.error("Unrecognized flag: " + args); process.exit(1); }
`);
  await chmod(binary, 0o700);
  assert.equal(await locateOpenCode({ PATH: root }, "linux"), binary);
  assert.deepEqual(await installedOpenCode(binary), { installed: true, version: "2.2.5" });
  const catalog = await inspectOpenCode(binary);
  assert.equal(catalog.installed, true);
  assert.equal(catalog.defaultModel, "opencode/beam-free", "the project document after the global one wins");
  assert.deepEqual(catalog.models.map(m => [m.id, m.free, m.reasoning]), [
    ["openai/gpt-6.1-terra", false, true], ["opencode/beam-free", true, true], ["opencode/deepseek-v4.1-flash-free", true, true],
    ["google/gemma-4-31b-it", true, true], ["opencode/big-pickle", true, false], ["openai/gpt-4o", false, false],
  ], "deprecated and disabled models are left out");
  assert.equal(catalog.models[1].name, "Beam Free");
  assert.ok(catalog.models.every(m => Number.isFinite(m.context)));
  assert.equal(JSON.stringify(catalog).includes("synthetic-secret"), false);
  assert.equal(Object.hasOwn(catalog, "providers"), false);
  assert.equal(await locateOpenCode({ WONDER_OPENCODE_BIN: join(root, "missing"), PATH: root }), null);
});

test("model listing and default parse OpenCode 2's shapes", async () => {
  const models = parseModels(await fixture("api-model-list.json"));
  const by = Object.fromEntries(models.map(m => [m.id, m]));
  assert.equal(by["openai/gpt-6.1-terra"].free, false, "tiered paid cost is not free");
  assert.equal(by["opencode/big-pickle"].free, true);
  assert.equal(by["opencode/big-pickle"].reasoning, false, "no variants, no reasoning");
  assert.equal(parseModels(JSON.stringify({ data: [{ providerID: "x", id: "y-fast", modelID: "y" }] }))[0].id, "x/y-fast", "the variant id selects the model");
  assert.equal(parseModels(JSON.stringify({ data: [{ providerID: "x", id: "y" }] }))[0].name, "x/y", "no name falls back to the id");
  assert.throws(() => parseModels("[]"), SyntaxError);
  assert.equal(configuredModel(await fixture("debug-config.json")), "opencode/beam-free");
  assert.equal(configuredModel(JSON.stringify([{ type: "document", info: { model: "a/b" } }, { type: "document", info: { model: { providerID: "c", model: "d" } } }, { type: "document", info: {} }])), "c/d");
  assert.equal(configuredModel(JSON.stringify([{ type: "document", info: { model: "a/b" } }])), "a/b", "the string form is accepted");
  assert.equal(configuredModel(JSON.stringify([{ type: "directory", path: "/x" }])), null);
  assert.throws(() => configuredModel("{}"), SyntaxError, "the pre-2.x object form is not read");
});

// npm's own cmd-shim, as `npm install -g opencode-ai` writes it on Windows:
// an extensionless shell script, `.cmd` and `.ps1` beside the package.
const npmCmdShim = (() => {
  const npm = [join(dirname(process.execPath), "node_modules", "npm"), join(dirname(process.execPath), "..", "lib", "node_modules", "npm")];
  for (const root of npm) { try { return createRequire(join(root, "package.json"))("cmd-shim"); } catch {} }
  return null;
})();

test("Windows discovery runs an npm-installed OpenCode's package script, not its shell shim",
  { skip: npmCmdShim ? false : "npm's cmd-shim is not installed beside this Node" }, async t => {
  const root = await mkdtemp(join(tmpdir(), "wonder-opencode-npm-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const script = join(root, "node_modules", "opencode-ai", "bin", "opencode");
  await mkdir(dirname(script), { recursive: true });
  // npm install marks a package's bin executable; newer cmd-shim no longer does it for us.
  await writeFile(script, `#!/usr/bin/env node\nconsole.log("2.2.5");\n`, { mode: 0o755 });
  await npmCmdShim(script, join(root, "opencode"));
  assert.match(await readFile(join(root, "opencode"), "utf8"), /^#!\/bin\/sh/, "the fixture is npm's shell shim");
  const found = await locateOpenCode({ PATH: root }, "win32");
  assert.equal(found, script);
  const { command, args } = openCodeCommand(found, ["--version"], "win32");
  assert.equal((await promisify(execFile)(command, args)).stdout.trim(), "2.2.5");
});

test("a missing or broken CLI does not claim installation", async () => {
  assert.deepEqual(await inspectOpenCode(null), { installed: false, version: null, models: [], missing: true });
  assert.deepEqual(await installedOpenCode(process.execPath + "-missing"), { installed: false, version: null });
});

test("a CLI that ignores termination cannot keep discovery open", { timeout: 15_000 }, async t => {
  const root = await mkdtemp(join(tmpdir(), "wonder-opencode-stalled-catalog-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const binary = join(root, "opencode"), receipt = join(root, "peer.pid");
  await writeFile(binary, `#!${process.execPath}
const {writeFileSync} = require("node:fs");
process.on("SIGTERM", () => {});
writeFileSync(${JSON.stringify(receipt)}, String(process.pid));
setInterval(() => {}, 1000);
`, { mode: 0o700 });
  assert.deepEqual(await installedOpenCode(binary), { installed: false, version: null });
  const pid = Number(await readFile(receipt, "utf8"));
  assert.throws(() => process.kill(pid, 0), { code: "ESRCH" }, "The timed-out helper must have exited");
});

// Cursor CLI aliases are not its ACP configuration IDs. The catalog must come
// from authenticated ACP state without submitting a prompt or retaining auth.
test("Cursor discovery reads native ACP models without prompts or retaining credentials", async t => {
  const root = await mkdtemp(join(tmpdir(), "wonder-cursor-catalog-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const binary = join(root, "agent"), controls = join(root, "controls.json");
  await writeFile(binary, `#!${process.execPath}
if (process.argv[2] === "--version") console.log("2026.10.16-e3cb207");
else if (process.argv[2] === "status") console.log(JSON.stringify({isAuthenticated:true, accessToken:"synthetic-secret", userInfo:{email:"private@example.invalid"}}));
else if (process.argv[2] === "acp") {
  process.env.WONDER_FAKE_ACP_PROVIDER = "cursor";
  process.env.WONDER_FAKE_ACP_CONTROLS = ${JSON.stringify(controls)};
  await import(${JSON.stringify(new URL("./fake-agent.mjs", import.meta.url).href)});
} else process.exit(2);
`, { mode: 0o700 });
  assert.equal(await locateCursor({ WONDER_CURSOR_BIN: binary }), binary);
  assert.equal(await locateCursor({ PATH: root }, "win32"), null, "Windows must not execute a POSIX shell shim");
  const first = await inspectCursor(binary, root, { cursorHome: join(root, "cursor-home") });
  assert.equal(first.connected, true);
  assert.equal(first.defaultModel, "default[]");
  assert.equal(first.models[1].id, "gpt-5.6-luna[context=272k,reasoning=medium,fast=false]");
  assert.equal(cursorAdapter.nativeModel("cursor:" + first.models[1].id), first.models[1].id);
  const native = JSON.parse(await readFile(controls, "utf8"));
  assert.deepEqual(native.map(control => control.method), ["authenticate", "session/new"]);
  assert.deepEqual(await inspectCursor(binary, root, { cursorHome: join(root, "cursor-home") }), first);
  assert.deepEqual(JSON.parse(await readFile(controls, "utf8")).map(control => control.method), ["authenticate", "session/new"]);
  await assert.rejects(readFile(join(root, "catalog-session.json")), { code: "ENOENT" });
  assert.doesNotMatch(JSON.stringify(first), /synthetic-secret|private@example/);
});

// Cursor keeps every ACP session it starts, and its helper outlives `agent acp`.
// Each model check must leave neither behind, and must not touch other sessions.
test("Cursor model check removes its own probe sessions and helper process, nothing else", { timeout: 15000 }, async t => {
  const root = await mkdtemp(join(tmpdir(), "wonder-cursor-probe-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const runtime = join(root, "cursor-runtime"), home = join(root, "cursor-home"), sessions = join(home, "acp-sessions");
  await mkdir(runtime);
  const folder = (id, cwd) => mkdir(join(sessions, id), { recursive: true }).then(() => writeFile(join(sessions, id, "meta.json"), JSON.stringify({ cwd })));
  const earlier = "11111111-2222-4333-8444-555555555555", user = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
  await folder(earlier, runtime);
  await folder(user, join(root, "project"));
  await folder("not-a-session", runtime);
  const binary = join(root, "agent"), workerPid = join(root, "worker.pid");
  await writeFile(binary, `#!${process.execPath}
if (process.argv[2] === "--version") console.log("2026.10.16-e3cb207");
else if (process.argv[2] === "status") console.log(JSON.stringify({isAuthenticated:true}));
else if (process.argv[2] === "acp") {
  const { mkdirSync, writeFileSync } = await import("node:fs");
  const { spawn } = await import("node:child_process");
  const { randomUUID } = await import("node:crypto");
  const folder = ${JSON.stringify(sessions)} + "/" + randomUUID();
  mkdirSync(folder, { recursive: true });
  writeFileSync(folder + "/meta.json", JSON.stringify({ cwd: process.cwd() }));
  const worker = spawn(process.execPath, ["-e", "process.on('SIGHUP',()=>{});setInterval(()=>{},1000)"], { stdio: "ignore" });
  worker.unref();
  writeFileSync(${JSON.stringify(workerPid)}, String(worker.pid));
  process.env.WONDER_FAKE_ACP_PROVIDER = "cursor";
  await import(${JSON.stringify(new URL("./fake-agent.mjs", import.meta.url).href)});
} else process.exit(2);
`, { mode: 0o700 });
  const catalog = await inspectCursor(binary, runtime, { cursorHome: home });
  assert.equal(catalog.connected, true);
  assert.ok(catalog.models.length > 0);
  const pid = Number(await readFile(workerPid, "utf8"));
  t.after(() => { try { process.kill(pid, "SIGKILL"); } catch {} });
  assert.throws(() => process.kill(pid, 0), { code: "ESRCH" }, "the helper Cursor started must not outlive the check");
  assert.deepEqual((await readdir(sessions)).sort(), ["aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee", "not-a-session"],
    "this check's session and earlier probe sessions go; a Project session and unrelated folders stay");
});

test("Cursor installation and sign-in are distinct; signed-out discovery starts no ACP session", async t => {
  assert.deepEqual(await installedCursor(null), { installed: false, version: null, connected: false, missing: true });
  const root = await mkdtemp(join(tmpdir(), "wonder-cursor-signed-out-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const binary = join(root, "agent");
  await writeFile(binary, `#!${process.execPath}
if (process.argv[2] === "--version") console.log("2026.10.16-e3cb207");
else if (process.argv[2] === "status") console.log(JSON.stringify({isAuthenticated:false}));
else { (await import("node:fs/promises")).writeFile(${JSON.stringify(join(root, "unexpected-start"))}, "bad"); process.exitCode=1; }
`, { mode: 0o700 });
  assert.deepEqual(await inspectCursor(binary, root), { installed: true, version: "2026.10.16-e3cb207", connected: false, models: [] });
  await assert.rejects(readFile(join(root, "unexpected-start")), { code: "ENOENT" });
});

test("Cursor model discovery preserves grouped opaque IDs and has no invented fallback models", () => {
  assert.deepEqual(cursorModels({}), { models: [], defaultModel: null });
  const id = "native-model[context=1m,effort=high,fast=false]";
  assert.deepEqual(cursorModels({ configOptions: [{ category: "model", type: "select", currentValue: id,
    options: [{ group: "native", options: [{ value: id, name: "Native" }] }] }] }),
    { models: [{ id, name: "Native", providerId: "cursor" }], defaultModel: id });
  assert.throws(() => cursorAdapter.nativeModel("opencode:" + id), /Cursor model/);
  assert.throws(() => cursorAdapter.nativeModel("cursor:bad\nmodel"), /Cursor model/);
});

test("Cursor model names pass through for the daemon to name", () => {
  // Rows as Cursor 2026.10 reports them: the name may repeat the ID's base or be missing.
  const rows = [["default[]", "Auto", "Auto"], ["gpt-5.6-sol[context=272k,reasoning=medium,fast=false]", "gpt-5.6-sol", "gpt-5.6-sol"],
    ["composer-2.5[fast=true]", undefined, "composer-2.5[fast=true]"], ["gpt-5.6-luna[x=1]", "GPT-5.6 Luna", "GPT-5.6 Luna"]];
  const { models } = cursorModels({ configOptions: [{ category: "model", type: "select", options: rows.map(([value, name]) => ({ value, name })) }] });
  assert.deepEqual(models.map(m => [m.id, m.name]), rows.map(([id, , name]) => [id, name]));
  assert.deepEqual(cursorModels({ models: { availableModels: [{ modelId: "grok-4.7[fast=true]", name: "grok-4.7" }, { modelId: "x_y", name: "x_y" }] } }).models.map(m => m.name),
    ["grok-4.7", "x_y"]);
});

test("Cursor discovery cancellation ends its owned CLI probes before ACP can start", { timeout: 15000 }, async t => {
  const root = await mkdtemp(join(tmpdir(), "wonder-cursor-cancel-discovery-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const binary = join(root, "agent");
  await writeFile(binary, `#!${process.execPath}
const { writeFileSync } = await import("node:fs");
const { join } = await import("node:path");
writeFileSync(join(${JSON.stringify(root)}, process.argv[2] === "--version" ? "version.pid" : "status.pid"), String(process.pid));
process.on("SIGTERM", () => {});
setInterval(() => {}, 1000);
`, { mode: 0o700 });
  const abort = new AbortController();
  const discovery = inspectCursor(binary, root, { signal: abort.signal });
  const started = performance.now();
  const pids = [];
  for (const name of ["version.pid", "status.pid"]) {
    let pid;
    while (!pid) {
      pid = await readFile(join(root, name), "utf8").then(Number).catch(() => null);
      // Only a hang guard for the two fake probes starting up (two Node launches);
      // the contract is checked after abort below.
      assert.ok(performance.now() - started < 10000);
      if (!pid) await new Promise(resolve => setTimeout(resolve, 10));
    }
    pids.push(pid);
  }
  abort.abort();
  await assert.rejects(discovery, { name: "AbortError" });
  for (const pid of pids) assert.throws(() => process.kill(pid, 0), { code: "ESRCH" });
});

test("Cursor phone status uses native sign-in without exposing credentials or starting ACP", async t => {
  const root = await mkdtemp(join(tmpdir(), "wonder-cursor-phone-status-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const binary = join(root, "agent");
  await writeFile(binary, `#!${process.execPath}
if (process.argv[2] === "--version") console.log("2026.10.16-fixture");
else if (process.argv[2] === "status") console.log(JSON.stringify({isAuthenticated:process.env.CURSOR_TEST_SIGNED_IN === "1", accessToken:"synthetic-secret", userInfo:{email:"private@example.invalid"}}));
else { (await import("node:fs/promises")).writeFile(${JSON.stringify(join(root, "unexpected-start"))}, "bad"); process.exitCode=1; }
`, { mode: 0o700 });
  for (const [signedIn, code, account] of [["1", 0, "Cursor account"], ["0", 42, ""]]) {
    const result = await promisify(execFile)(process.execPath, [new URL("../catalog.mjs", import.meta.url).pathname, "cursor-status"],
      { env: { ...process.env, WONDER_CURSOR_BIN: binary, CURSOR_TEST_SIGNED_IN: signedIn } })
      .then(output => ({ ...output, code: 0 }), error => error);
    assert.equal(result.code, code);
    assert.equal(result.stdout, `2026.10.16-fixture\t${account}\n`);
    assert.equal(result.stderr, "");
    assert.doesNotMatch(result.stdout, /synthetic-secret|private@example/);
  }
  await assert.rejects(readFile(join(root, "unexpected-start")), { code: "ENOENT" });
});

// Work mode and permission policy are separate contracts. A work mode is a
// native agent choice (never Cursor's Ask, a question-only chat), and no
// adapter can set permissions: anything but the neutral stored values fails
// instead of being mapped onto a mode.
test("work modes never stand in for permission levels", () => {
  for (const [adapter, modes] of [[openCodeAdapter, { default: "build", plan: "plan" }], [cursorAdapter, { default: "agent", plan: "plan" }]]) {
    assert.deepEqual(adapter.permissionProfiles.map(profile => ({ ...profile })), [{ id: "native", enforcement: "native" }],
      `${adapter.label} has no Cupola permission setter`);
    assert.deepEqual({ ...adapter.workModes }, modes);
    const neutral = [undefined, null, {}, { mode: "workspace", approvalMode: "ask" },
      { mode: "workspace", approvalMode: "ask", planMode: true, writeRoots: ["/project"], deniedRoots: [] }];
    // Older hosts' neutral "ask", or no value at all, is the agent's own
    // settings: never an enforced Ask.
    for (const policy of neutral) {
      assert.doesNotThrow(() => adapter.validatePolicy(policy));
      assert.equal(adapter.permissionProfile(policy).enforcement, "native", JSON.stringify(policy));
    }
    assert.deepEqual(adapter.sessionSelections("m", { planMode: true }), [["model", "m"], ["mode", "plan"]]);
    assert.deepEqual(adapter.sessionSelections("m", undefined), [["model", "m"], ["mode", modes.default]]);
    const strict = [{ mode: "read_only" }, { mode: "read_only", planMode: true }, { mode: "full_access" }, { mode: "ask" }, { mode: "plan" },
      { approvalMode: "auto" }, { approvalMode: "accept_edits" }, { approvalMode: "full_access" }, { approvalMode: "plan" }, "read_only", ["ask"]];
    for (const policy of strict) {
      assert.throws(() => adapter.permissionProfile(policy), new RegExp(`Change permissions in ${adapter.label} settings`), JSON.stringify(policy));
      assert.throws(() => adapter.validatePolicy(policy), new RegExp(`Change permissions in ${adapter.label} settings`), JSON.stringify(policy));
    }
    for (const planMode of ["ask", "plan", "true", 1, {}]) {
      assert.throws(() => adapter.validatePolicy({ planMode }), /Turn Plan on or off/, JSON.stringify(planMode));
      assert.throws(() => adapter.sessionSelections("m", { planMode }), /Turn Plan on or off/);
    }
    assert.throws(() => { adapter.workModes.plan = "ask"; }, TypeError);
    assert.throws(() => { adapter.validatePolicy = () => {}; }, TypeError);
    assert.throws(() => { adapter.permissionProfiles[0].enforcement = "session"; }, TypeError);
  }
});

// OpenCode's commands share one SQLite database. The fake fails like the real
// CLI ("database is locked") whenever a second command opens it while another
// holds it, and records every launch.
async function lockingOpenCode(t, { listing = "ok", config = "ok" } = {}) {
  const root = await mkdtemp(join(tmpdir(), "wonder-opencode-locking-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const binary = join(root, "opencode"), log = join(root, "launches.log"), lock = join(root, "db.lock");
  await writeFile(binary, `#!${process.execPath}
const { appendFileSync, existsSync, readFileSync, rmSync, writeFileSync } = require("node:fs");
const args = process.argv.slice(2).join(" ");
appendFileSync(${JSON.stringify(log)}, args + "\\n");
if (args === "--version") { console.log("2.2.5"); process.exit(0); }
const fail = () => { console.error("Error: Unexpected error\\nError: database is locked"); process.exit(1); };
if (existsSync(${JSON.stringify(lock)})) { appendFileSync(${JSON.stringify(log)}, "overlap\\n"); fail(); }
writeFileSync(${JSON.stringify(lock)}, args);
const tries = readFileSync(${JSON.stringify(log)}, "utf8").split("\\n").filter(line => line === args).length;
setTimeout(() => {
  rmSync(${JSON.stringify(lock)});
  const mode = args === "api model.list" ? ${JSON.stringify(listing)} : ${JSON.stringify(config)};
  if (mode === "locked" || (mode === "locked-once" && tries === 1)) fail();
  if (mode === "broken") { console.error("Error: provider failed"); process.exit(3); }
  if (args === "debug config") console.log(JSON.stringify([{ type: "document", path: "/x", info: { model: { providerID: "local", model: "chosen" }, providers: { x: { apiKey: "synthetic-secret" } } } }]));
  else console.log(JSON.stringify({ data: [{ providerID: "local", id: "chosen", name: "Chosen" }] }));
}, 150);
`, { mode: 0o700 });
  const launches = async () => (await readFile(log, "utf8").catch(() => "")).split("\n").filter(Boolean);
  return { binary, launches };
}

test("OpenCode checks never open its database together", { timeout: 15_000 }, async t => {
  const { binary, launches } = await lockingOpenCode(t);
  const catalog = await inspectOpenCode(binary);
  assert.deepEqual(catalog.models.map(m => m.id), ["local/chosen"]);
  assert.equal(catalog.defaultModel, "local/chosen");
  assert.equal(catalog.modelsError, undefined);
  assert.deepEqual(await launches(), ["--version", "api model.list", "debug config"], "one at a time, no lock failures");
});

test("a locked OpenCode database is retried once; other failures are reported, not read as no models", { timeout: 20_000 }, async t => {
  const once = await lockingOpenCode(t, { listing: "locked-once" });
  assert.deepEqual((await inspectOpenCode(once.binary)).models.map(m => m.id), ["local/chosen"]);
  assert.equal((await once.launches()).filter(line => line === "api model.list").length, 2);

  const locked = await lockingOpenCode(t, { listing: "locked", config: "locked" });
  const stuck = await inspectOpenCode(locked.binary);
  assert.deepEqual(stuck.models, []);
  assert.match(stuck.modelsError, /^exit 1: Error: database is locked$/);
  assert.equal(stuck.configError, "exit 1", "configuration output and errors are never retained");
  assert.equal((await locked.launches()).filter(line => line === "api model.list").length, 2, "one retry, no more");

  const broken = await lockingOpenCode(t, { listing: "broken" });
  assert.match((await inspectOpenCode(broken.binary)).modelsError, /^exit 3: Error: provider failed$/);
  assert.equal((await broken.launches()).filter(line => line === "api model.list").length, 1, "only a lock is retried");
  assert.doesNotMatch(JSON.stringify([stuck]), /synthetic-secret/);
});

test("cancelling an OpenCode check ends the command it is running", { timeout: 15_000 }, async t => {
  const root = await mkdtemp(join(tmpdir(), "wonder-opencode-cancel-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const binary = join(root, "opencode"), receipt = join(root, "models.pid");
  await writeFile(binary, `#!${process.execPath}
if (process.argv[2] === "--version") { console.log("2.2.5"); process.exit(0); }
require("node:fs").writeFileSync(${JSON.stringify(receipt)}, String(process.pid));
process.on("SIGTERM", () => {});
setInterval(() => {}, 1000);
`, { mode: 0o700 });
  const abort = new AbortController();
  const check = inspectOpenCode(binary, { signal: abort.signal });
  let pid;
  for (const started = Date.now(); !pid && Date.now() - started < 10_000;) {
    pid = Number(await readFile(receipt, "utf8").catch(() => 0));
    if (!pid) await new Promise(resolve => setImmediate(resolve));
  }
  abort.abort(new Error("closing"));
  await assert.rejects(check, /closing/);
  assert.throws(() => process.kill(pid, 0), { code: "ESRCH" }, "the listing command must have exited");
  assert.equal(await readFile(join(root, "debug.pid"), "utf8").catch(() => null), null);
});

async function bridgeWith(t, { inspect, createAgent, adapter = openCodeAdapter } = {}) {
  const root = await mkdtemp(join(tmpdir(), "wonder-acp-availability-"));
  const sessions = await new Sessions(join(root, "sessions"), adapter).initialize();
  const frames = [];
  const bridge = new AcpBridge({ sessions, adapter, tools: false, send: async frame => { frames.push(frame); }, inspect, createAgent });
  t.after(async () => { await bridge.close(); await rm(root, { recursive: true, force: true }); });
  const call = async (method, params = {}) => {
    const id = `c-${frames.length}`;
    await bridge.receive({ id, method, params });
    const reply = frames.find(frame => frame.id === id);
    if (reply.error) throw new Error(reply.error.message);
    return reply.result;
  };
  return { bridge, call, root };
}

test("a failed model check keeps the last good models and default; only confirmed answers change availability", async t => {
  const good = { installed: true, version: "2.2.5", defaultModel: "local/chosen", models: [{ id: "local/other", name: "Other", providerId: "local" }, { id: "local/chosen", name: "Chosen", providerId: "local" }] };
  const answers = [good, { installed: true, version: "2.2.5", defaultModel: null, configError: "exit 1", models: [], modelsError: "exit 1: Error: database is locked" }];
  let next = 0;
  const { call } = await bridgeWith(t, { inspect: async () => {
    const answer = answers[Math.min(next++, answers.length - 1)];
    if (answer instanceof Error) throw answer;
    return answer;
  } });
  assert.equal((await call("account/read", { refresh: true })).availability, "ready");
  const failed = await call("account/read", { refresh: true });
  assert.equal(failed.connected, true, "a failed listing must not read as Install OpenCode");
  assert.equal(failed.availability, "ready");
  assert.equal(failed.stale, true);
  assert.equal(failed.diagnostic, "exit 1: Error: database is locked");
  const models = await call("model/list");
  assert.deepEqual(models.data.map(m => [m.id, m.isDefault]), [["opencode:local/chosen", true], ["opencode:local/other", false]]);

  answers.push(new Error("inspection crashed"));
  assert.equal((await call("account/read", { refresh: true })).connected, true, "a check that could not finish keeps the last good one");
  answers.push({ installed: true, version: "2.2.5", defaultModel: null, models: [] });
  assert.equal((await call("account/read", { refresh: true })).availability, "noModels", "an empty list that was read is an answer");
  answers.push({ installed: false, version: null, models: [], missing: true });
  const missing = await call("account/read", { refresh: true });
  assert.deepEqual([missing.connected, missing.availability], [false, "notInstalled"]);
});

test("availability tells a missing, signed-out or unchecked provider from one with no models", async t => {
  const cases = [
    [{ installed: true, version: "1", models: [], modelsError: "timed out" }, "unavailable"],
    [{ installed: false, version: null, models: [], modelsError: "version check failed" }, "unavailable"],
    [{ installed: false, version: null, connected: false, models: [], missing: true }, "notInstalled"],
    [{ installed: true, version: "1", connected: false, models: [] }, "signedOut"],
    [{ installed: true, version: "1", connected: false, statusFailed: true, models: [] }, "unavailable"],
    [{ installed: true, version: "1", connected: true, models: [] }, "noModels"],
  ];
  for (const [catalog, expected] of cases) {
    const { call } = await bridgeWith(t, { adapter: catalog.connected === undefined ? openCodeAdapter : cursorAdapter, inspect: async () => catalog });
    const account = await call("account/read", { refresh: true });
    assert.equal(account.availability, expected, JSON.stringify(catalog));
    assert.equal(account.connected, false);
  }
});

test("a model check waits for an agent's startup, and a start refused by a locked database is tried once more", { timeout: 15_000 }, async t => {
  const events = [];
  const fake = fileURLToPath(new URL("./fake-agent.mjs", import.meta.url));
  const starts = [];
  const { bridge } = await bridgeWith(t, {
    inspect: async () => {
      events.push("check begins");
      // Long enough for an unguarded agent start to begin meanwhile.
      for (let i = 0; i < 10; i++) await new Promise(resolve => setImmediate(resolve));
      events.push("check ends");
      return { installed: true, version: "1", models: [{ id: "local/chosen", name: "Chosen", providerId: "local" }] };
    },
    createAgent: handlers => {
      const locked = starts.length === 0;
      starts.push(locked ? "locked" : "ok");
      const agent = new AcpAgent({ command: process.execPath, label: "OpenCode", ...handlers,
        args: locked ? ["-e", "console.error('Error: database is locked'); process.exit(1)"] : [fake] });
      const start = agent.start.bind(agent);
      agent.start = async options => {
        events.push("start begins");
        try { return await start(options); } finally { events.push("start ends"); }
      };
      return agent;
    },
  });
  await bridge.catalog();
  events.length = 0;
  const agent = bridge.agent();
  await new Promise(resolve => setImmediate(resolve));
  const check = bridge.catalog(true);
  assert.equal((await agent).alive, true);
  await check;
  assert.deepEqual(starts, ["locked", "ok"]);
  assert.equal(events.length, 6);
  for (let i = 0; i < events.length; i += 2)
    assert.deepEqual([events[i].split(" ")[0], events[i + 1]], [events[i + 1].split(" ")[0], events[i].replace("begins", "ends")],
      `the refreshed check opens nothing while an agent is starting: ${events.join(", ")}`);

  // Any other startup failure is reported at once.
  await bridge.agentEntry.agent.close();
  starts.length = 0;
  const failing = await bridgeWith(t, { inspect: async () => ({ installed: true, version: "1", models: [{ id: "a/b", name: "B", providerId: "a" }] }),
    createAgent: handlers => { starts.push("broken"); return new AcpAgent({ command: process.execPath, args: ["-e", "console.error('Error: no config'); process.exit(1)"], ...handlers }); } });
  await assert.rejects(failing.bridge.agent());
  assert.deepEqual(starts, ["broken"]);
});

test("a reply never waits for a model check, warm or cold; starting needs only the located CLI", { timeout: 15_000 }, async t => {
  const fake = fileURLToPath(new URL("./fake-agent.mjs", import.meta.url));
  let checks = 0, located = process.execPath;
  const binaries = [];
  const { bridge } = await bridgeWith(t, {
    inspect: async () => { checks++; throw new Error("a reply ran a model check"); },
    createAgent: (handlers, { binary }) => {
      binaries.push(binary);
      return new AcpAgent({ command: binary, args: [fake], label: "OpenCode", ...handlers });
    },
  });
  bridge.locate = async () => located;
  // Cold: no check has ever run.
  const agent = await bridge.agent();
  assert.equal(agent.alive, true);
  // Warm, long after any cache would have expired.
  bridge.now = () => Date.now() + 24 * 60 * 60_000;
  assert.equal(await bridge.agent(), agent);
  assert.equal(checks, 0);
  assert.deepEqual(binaries, [process.execPath]);

  await agent.close();
  located = null;
  await assert.rejects(bridge.agent(), /Install OpenCode’s command-line tool on your computer, then reconnect OpenCode\./);
  assert.equal(checks, 0);
});

test("routine reads reuse the last check until it goes stale or the CLI or its settings change; refresh checks now", async t => {
  let checks = 0, clock = 0, fingerprint = "v1";
  const ready = { installed: true, version: "1", defaultModel: "local/chosen", models: [{ id: "local/chosen", name: "Chosen", providerId: "local" }] };
  const { bridge, call } = await bridgeWith(t, { inspect: async () => { checks++; return ready; } });
  Object.assign(bridge, { now: () => clock, fingerprint: async () => fingerprint });
  const routine = async () => { await call("account/read", {}); await call("model/list", {}); };

  await routine();
  assert.equal(checks, 1, "the first read checks");
  // The daemon's periodic check every five minutes for 55 minutes.
  for (let minute = 5; minute < 60; minute += 5) { clock = minute * 60_000; await routine(); }
  assert.equal(checks, 1, "unchanged and fresh: no check");
  clock = 61 * 60_000;
  await routine();
  assert.equal(checks, 2, "a bounded hourly check covers changes nothing local shows");

  fingerprint = "v2";
  await routine();
  assert.equal(checks, 3, "an updated CLI or a settings change checks at once");
  await routine();
  assert.equal(checks, 3);
  await call("account/read", { refresh: true });
  assert.equal(checks, 4, "an explicit refresh checks now");

  // A file the CLI rewrites during its own check is not a change.
  bridge.inspect = async () => { checks++; fingerprint = `rewritten-${checks}`; return ready; };
  await call("account/read", { refresh: true });
  await routine();
  assert.equal(checks, 5, "the check's own writes do not trigger another");
  bridge.inspect = async () => { checks++; return ready; };

  // Many callers at once share one check.
  await Promise.all([...Array(8)].map(() => call("account/read", { refresh: true })));
  assert.equal(checks, 6);
});

test("a provider that is not set up is re-read slowly, and a failed check backs off", async t => {
  let clock = 0;
  const answers = [];
  let checks = 0;
  const { bridge, call } = await bridgeWith(t, { adapter: cursorAdapter, inspect: async () => {
    checks++;
    const answer = answers.shift();
    if (answer instanceof Error) throw answer;
    return answer;
  } });
  bridge.now = () => clock;
  answers.push({ installed: true, version: "1", connected: false, models: [] });
  assert.equal((await call("account/read", {})).availability, "signedOut");
  clock = 25 * 60_000;
  assert.equal((await call("account/read", {})).availability, "signedOut");
  assert.equal(checks, 1, "signed out: no check every five minutes");
  answers.push({ installed: true, version: "1", connected: true, models: [{ id: "auto", name: "Auto", providerId: "cursor" }] });
  clock = 31 * 60_000;
  assert.equal((await call("account/read", {})).availability, "ready");
  assert.equal(checks, 2);

  // Failed checks keep the last good models and retry from 30 s, doubling.
  const failedAt = [];
  for (let i = 0; i < 4; i++) answers.push(new Error("status timed out"));
  clock = 92 * 60_000;
  for (let step = 0; step < 20 * 60; step += 5) {
    const before = checks;
    const account = await call("account/read", {});
    assert.equal(account.availability, "ready", "a failed check does not change availability");
    if (checks > before) failedAt.push(clock);
    if (failedAt.length === 4) break;
    clock += 5_000;
  }
  const gaps = failedAt.slice(1).map((at, i) => (at - failedAt[i]) / 1000);
  assert.equal(gaps.length, 3);
  for (const [gap, base] of gaps.map((gap, i) => [gap, 30 * 2 ** i])) assert.ok(gap >= base * 0.8 && gap <= base * 1.2 + 5, `retry gaps ${gaps}`);
  answers.push({ installed: true, version: "1", connected: true, models: [{ id: "auto", name: "Auto", providerId: "cursor" }] });
  const before = checks;
  assert.equal((await call("account/read", { refresh: true })).stale, false, "Reconnect wakes a backed-off check");
  assert.equal(checks, before + 1);
});

test("the setup fingerprint notices an installed, updated or moved CLI and settings changes, reading no file", async t => {
  const root = await mkdtemp(join(tmpdir(), "wonder-acp-fingerprint-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const env = { XDG_CONFIG_HOME: join(root, "config"), XDG_DATA_HOME: join(root, "data") };
  const files = settingsFiles("opencode", env, root);
  assert.ok(files.includes(join(root, "data", "opencode", "auth.json")));
  assert.ok(settingsFiles("cursor", env, root).includes(join(root, ".cursor", "cli-config.json")));
  const binary = join(root, "opencode");
  const none = await setupFingerprint(null, files);
  await writeFile(binary, "#!/bin/sh\n", { mode: 0o755 });
  const installed = await setupFingerprint(binary, files);
  assert.notEqual(installed, none);
  assert.equal(await setupFingerprint(binary, files), installed, "nothing changed");
  await writeFile(binary, "#!/bin/sh\necho 2\n", { mode: 0o755 });
  const updated = await setupFingerprint(binary, files);
  assert.notEqual(updated, installed);
  await mkdir(join(root, "data", "opencode"), { recursive: true });
  await writeFile(join(root, "data", "opencode", "auth.json"), "{\"secret\":\"never read\"}");
  const signedIn = await setupFingerprint(binary, files);
  assert.notEqual(signedIn, updated);
  assert.doesNotMatch(signedIn, /never read/);
});

test("a cold reply does not wait behind a model check: the check yields, then runs again", { timeout: 15_000 }, async t => {
  const fake = fileURLToPath(new URL("./fake-agent.mjs", import.meta.url));
  const events = [];
  let checks = 0;
  const { bridge, call } = await bridgeWith(t, {
    // A check that would take far longer than a reply may wait, unless cancelled.
    inspect: ({ signal } = {}) => {
      const check = ++checks;
      events.push(`check ${check} begins`);
      return new Promise((resolve, reject) => {
        const timer = setTimeout(() => { events.push(`check ${check} ends`); resolve({ installed: true, version: "1", models: [{ id: "local/chosen", name: "Chosen", providerId: "local" }] }); }, check === 1 ? 60_000 : 10);
        signal?.addEventListener("abort", () => { clearTimeout(timer); events.push(`check ${check} cancelled`); reject(signal.reason); }, { once: true });
      });
    },
    createAgent: handlers => {
      const agent = new AcpAgent({ command: process.execPath, args: [fake], label: "OpenCode", ...handlers });
      const start = agent.start.bind(agent);
      agent.start = async options => { events.push("start begins"); try { return await start(options); } finally { events.push("start ends"); } };
      return agent;
    },
  });
  const account = call("account/read", { refresh: true });
  await until(() => events.includes("check 1 begins"));
  const began = Date.now();
  const agent = await bridge.agent();
  assert.equal(agent.alive, true);
  assert.ok(Date.now() - began < 5000, "the reply did not wait for the check");
  // The check that yielded runs again after the start; its caller still gets an answer.
  assert.equal((await account).availability, "ready");
  assert.deepEqual(events, ["check 1 begins", "check 1 cancelled", "start begins", "start ends", "check 2 begins", "check 2 ends"],
    "the cancelled check ended before the start, and nothing overlapped");
});

test("concurrent cold replies start one agent", { timeout: 15_000 }, async t => {
  const fake = fileURLToPath(new URL("./fake-agent.mjs", import.meta.url));
  let created = 0;
  const { bridge } = await bridgeWith(t, {
    inspect: async () => { throw new Error("no check expected"); },
    createAgent: handlers => { created++; return new AcpAgent({ command: process.execPath, args: [fake], label: "OpenCode", ...handlers }); },
  });
  // Every reply is past its live-agent check before the CLI is found.
  const found = Promise.withResolvers();
  bridge.locate = async () => { await found.promise; return process.execPath; };
  const replies = [bridge.agent(), bridge.agent(), bridge.agent()];
  found.resolve();
  const agents = await Promise.all(replies);
  assert.equal(created, 1);
  assert.ok(agents.every(agent => agent === agents[0]));
});

async function until(check, ms = 5000) {
  const deadline = Date.now() + ms;
  while (!(await check())) {
    if (Date.now() >= deadline) throw new Error("test deadline exceeded");
    await new Promise(resolve => setImmediate(resolve));
  }
}

// Contract: a queued discovery must yield just like a running discovery, while
// preserving the single OpenCode database opener. No provider/model work runs.
test("a cold reply overtakes discovery admitted before the setup queue runs", { timeout: 15_000 }, async t => {
  const fake = fileURLToPath(new URL("./fake-agent.mjs", import.meta.url));
  const events = [];
  const inspected = Promise.withResolvers(), finishCheck = Promise.withResolvers();
  const { bridge } = await bridgeWith(t, {
    inspect: async ({ signal }) => {
      signal.throwIfAborted();
      events.push("check begins");
      inspected.resolve();
      await finishCheck.promise;
      signal.throwIfAborted();
      return { installed: true, version: "1", models: [{ id: "local/chosen", name: "Chosen", providerId: "local" }] };
    },
    createAgent: handlers => {
      const agent = new AcpAgent({ command: process.execPath, args: [fake], label: "OpenCode", ...handlers });
      const start = agent.start.bind(agent);
      agent.start = async options => {
        events.push("start begins");
        try { return await start(options); } finally { events.push("start ends"); }
      };
      return agent;
    },
  });
  const releaseQueue = Promise.withResolvers();
  const blocker = bridge.exclusive(() => releaseQueue.promise);
  const check = bridge.catalog(true);
  await until(() => Boolean(bridge.inspecting));
  const reply = bridge.agent();
  releaseQueue.resolve();
  try {
    // If discovery was not cancelled while queued, it reaches inspect first.
    await Promise.race([reply, inspected.promise]);
    assert.equal(events[0], "start begins", "the queued model check cannot take the reply's place");
    assert.equal((await reply).alive, true);
    await inspected.promise;
    assert.deepEqual(events, ["start begins", "start ends", "check begins"]);
  } finally {
    finishCheck.resolve();
    await Promise.allSettled([blocker, check, reply]);
  }
});
